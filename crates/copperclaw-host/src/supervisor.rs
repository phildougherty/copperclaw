//! Background-loop supervisor — M21 S1, architecture decision (g).
//!
//! Every host background loop (inbound consumer, delivery active + sweep
//! loops, sweep loop, typing ticker, todo watcher) used to be a bare
//! `tokio::spawn` whose `JoinHandle` was awaited only at shutdown
//! (`boot.rs`). A panic silently killed that subsystem for the remaining
//! life of the process — indistinguishable from idle, invisible to
//! logs-at-a-glance and doctor. This module owns those loops instead:
//!
//! - Each loop registers as a **named** task via a factory closure that can
//!   mint a fresh future for every (re)start.
//! - Panics and unexpected exits are caught, logged at ERROR, and the loop
//!   restarts on the decision-(e) backoff curve ([`BACKOFF_STEPS`]:
//!   5s -> 15s -> 60s -> 300s cap), with the streak resetting after
//!   [`HEALTHY_RESET_WINDOW`] (10 minutes) healthy.
//! - A loop whose restart streak exhausts the curve flips its own — and the
//!   supervisor-wide — degraded flag. Restarts continue at the cap so a
//!   subsystem that eventually heals still comes back; the flag clears once
//!   the loop stays healthy through the reset window.
//! - Loops keep their own internal error handling. The supervisor catches
//!   ONLY the class that previously killed a subsystem until process exit:
//!   panics and returns outside shutdown.
//!
//! Downstream consumers (the seams this card leaves for later M21 cards):
//!
//! - **O1 (`cclaw doctor`)** reads per-loop liveness + restart counts via
//!   the `host.status` admin-socket handler
//!   (`crate::handlers::host_status`), which is backed by
//!   [`SupervisorStatus::snapshot`].
//! - **O4 (operator alerts)** subscribes to the permanent-failure event via
//!   [`SupervisorStatus::degraded_watch`] — a live `tokio::sync::watch`
//!   channel that flips exactly when the supervisor-wide degraded flag
//!   changes.
//!
//! Shutdown semantics are unchanged: every loop still selects on the same
//! [`CancellationToken`] it always did and drains on SIGTERM; the driver
//! task returned by [`Supervisor::run`] exits once every registered loop
//! has drained, and boot awaits it inside the same 30-second deadline as
//! before.
//!
//! All timing goes through `tokio::time` (`Instant` / `sleep`) so tests can
//! drive the backoff curve with a paused clock — no real waits.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info};

/// Decision-(e) restart backoff curve. Restart `n` (1-based, within a
/// streak) waits `BACKOFF_STEPS[min(n - 1, len - 1)]` before the loop is
/// respawned; the last step is the cap.
pub const BACKOFF_STEPS: [Duration; 4] = [
    Duration::from_secs(5),
    Duration::from_secs(15),
    Duration::from_secs(60),
    Duration::from_secs(300),
];

/// A loop incarnation that stays healthy this long resets its restart
/// streak (the backoff curve starts over) and clears its degraded flag.
pub const HEALTHY_RESET_WINDOW: Duration = Duration::from_secs(600);

/// Boxed future one loop incarnation runs to completion.
type LoopFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Factory minting a fresh incarnation of a loop. Called once at startup
/// and once per restart.
type LoopFactory = Box<dyn FnMut() -> LoopFuture + Send>;

/// Why a supervised loop exited outside shutdown.
#[derive(Debug, Clone)]
enum ExitKind {
    /// The future returned normally while the host was still running —
    /// e.g. an inbound channel closing under the consumer.
    Returned,
    /// The task panicked; the payload message is carried when extractable.
    Panicked(String),
}

impl ExitKind {
    fn describe(&self) -> String {
        match self {
            Self::Returned => "returned unexpectedly".to_string(),
            Self::Panicked(msg) => format!("panicked: {msg}"),
        }
    }

    /// Low-cardinality reason token for the M21 S1 restart metric (the
    /// panic payload is deliberately excluded — it would explode label
    /// cardinality).
    fn metric_reason(&self) -> &'static str {
        match self {
            Self::Returned => "returned",
            Self::Panicked(_) => "panicked",
        }
    }
}

/// Per-loop bookkeeping. Times use `tokio::time::Instant` so a paused test
/// clock drives resets deterministically.
struct TaskState {
    name: &'static str,
    /// True while an incarnation is running (false during a backoff wait
    /// and after shutdown drain).
    alive: bool,
    /// Restarts over the process lifetime (monotonic).
    restarts_total: u64,
    /// Restarts since the last healthy reset; indexes the backoff curve.
    restart_streak: u32,
    /// The current (or most recent) incarnation's start time.
    started_at: Instant,
    /// True once `restart_streak` has exhausted [`BACKOFF_STEPS`].
    degraded: bool,
    /// Human-readable reason for the most recent unexpected exit.
    last_exit: Option<String>,
}

/// One row of [`SupervisorStatus::snapshot`] — the shape the `host.status`
/// admin-socket handler serialises for O1.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LoopStatus {
    pub name: String,
    pub alive: bool,
    pub restarts: u64,
    pub restart_streak: u32,
    pub degraded: bool,
    pub last_exit: Option<String>,
    /// Seconds the current incarnation has been running (0 while a restart
    /// is pending in backoff).
    pub uptime_secs: u64,
}

/// Shared, read-side view of the supervisor. Cloned (via `Arc`) into the
/// admin-socket `HandlerCtx` so `host.status` can report liveness without
/// touching the driver.
pub struct SupervisorStatus {
    tasks: Mutex<Vec<TaskState>>,
    degraded: tokio::sync::watch::Sender<bool>,
}

impl std::fmt::Debug for SupervisorStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SupervisorStatus")
            .field("degraded", &self.degraded())
            .finish_non_exhaustive()
    }
}

impl SupervisorStatus {
    fn new() -> Self {
        let (tx, _rx) = tokio::sync::watch::channel(false);
        Self {
            tasks: Mutex::new(Vec::new()),
            degraded: tx,
        }
    }

    /// Lock the task table, surviving poisoning: a panicked supervisor
    /// loop must not cascade into every status reader and mutator.
    fn tasks_guard(&self) -> std::sync::MutexGuard<'_, Vec<TaskState>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// True when any supervised loop has exhausted its backoff curve and
    /// not yet healed.
    pub fn degraded(&self) -> bool {
        *self.degraded.borrow()
    }

    /// Live subscription to the supervisor-wide degraded flag. This is the
    /// seam O4 hooks for the permanent-failure operator alert: the value
    /// flips to `true` exactly when a loop exceeds the restart cap and back
    /// to `false` once every degraded loop has healed.
    pub fn degraded_watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.degraded.subscribe()
    }

    /// Per-loop status rows, in registration order. Applies the lazy
    /// healthy-reset first so a loop that has been running past
    /// [`HEALTHY_RESET_WINDOW`] reports a cleared streak / degraded flag
    /// even if it never exits again.
    pub fn snapshot(&self) -> Vec<LoopStatus> {
        let now = Instant::now();
        let mut tasks = self.tasks_guard();
        for t in tasks.iter_mut() {
            heal_if_window_elapsed(t, now);
            // M21 S1 (M1 rider): refresh the gauges at status-read time so a
            // lazy healthy-reset (degraded → healthy without a further exit)
            // is reflected for scrapers, not just at the next transition.
            copperclaw_metrics::set_supervised_loop_alive(t.name, t.alive);
            copperclaw_metrics::set_supervised_loop_degraded(t.name, t.degraded);
        }
        let out = tasks
            .iter()
            .map(|t| LoopStatus {
                name: t.name.to_string(),
                alive: t.alive,
                restarts: t.restarts_total,
                restart_streak: t.restart_streak,
                degraded: t.degraded,
                last_exit: t.last_exit.clone(),
                uptime_secs: if t.alive {
                    now.duration_since(t.started_at).as_secs()
                } else {
                    0
                },
            })
            .collect();
        let any_degraded = tasks.iter().any(|t| t.degraded);
        drop(tasks);
        self.publish_degraded(any_degraded);
        out
    }

    /// Register a loop; returns its index. Driver-only.
    fn add_task(&self, name: &'static str) -> usize {
        let mut tasks = self.tasks_guard();
        tasks.push(TaskState {
            name,
            alive: false,
            restarts_total: 0,
            restart_streak: 0,
            started_at: Instant::now(),
            degraded: false,
            last_exit: None,
        });
        tasks.len() - 1
    }

    fn name(&self, idx: usize) -> &'static str {
        self.tasks_guard()[idx].name
    }

    /// A (re)started incarnation is now running.
    fn mark_started(&self, idx: usize, now: Instant) {
        let mut tasks = self.tasks_guard();
        let t = &mut tasks[idx];
        t.alive = true;
        t.started_at = now;
        // M21 S1 (M1 rider): per-loop liveness gauge.
        copperclaw_metrics::set_supervised_loop_alive(t.name, true);
    }

    /// The loop drained during shutdown — an expected exit.
    fn mark_stopped(&self, idx: usize) {
        let mut tasks = self.tasks_guard();
        tasks[idx].alive = false;
        // M21 S1 (M1 rider): per-loop liveness gauge.
        copperclaw_metrics::set_supervised_loop_alive(tasks[idx].name, false);
    }

    /// Record an unexpected exit and compute the restart backoff. Applies
    /// the healthy reset first (an incarnation that ran longer than
    /// [`HEALTHY_RESET_WINDOW`] starts a fresh streak), then advances the
    /// streak, flips the degraded flag once the streak exceeds the curve,
    /// and returns `(delay, restarts_total, degraded)`.
    fn record_unexpected_exit(
        &self,
        idx: usize,
        reason: String,
        now: Instant,
    ) -> (Duration, u64, bool) {
        let mut tasks = self.tasks_guard();
        let t = &mut tasks[idx];
        heal_if_window_elapsed(t, now);
        t.alive = false;
        t.last_exit = Some(reason);
        t.restarts_total += 1;
        t.restart_streak += 1;
        let step = (t.restart_streak as usize - 1).min(BACKOFF_STEPS.len() - 1);
        if t.restart_streak as usize > BACKOFF_STEPS.len() {
            t.degraded = true;
        }
        let out = (BACKOFF_STEPS[step], t.restarts_total, t.degraded);
        // M21 S1 (M1 rider): the incarnation is down until its backoff
        // respawn; publish liveness + degraded gauges for this loop.
        copperclaw_metrics::set_supervised_loop_alive(t.name, false);
        copperclaw_metrics::set_supervised_loop_degraded(t.name, t.degraded);
        let any_degraded = tasks.iter().any(|task| task.degraded);
        drop(tasks);
        self.publish_degraded(any_degraded);
        out
    }

    fn publish_degraded(&self, value: bool) {
        self.degraded.send_if_modified(|current| {
            if *current == value {
                false
            } else {
                *current = value;
                true
            }
        });
    }
}

/// Healthy reset: an incarnation that has run at least
/// [`HEALTHY_RESET_WINDOW`] clears its streak and degraded flag.
fn heal_if_window_elapsed(t: &mut TaskState, now: Instant) {
    if t.alive && now.duration_since(t.started_at) >= HEALTHY_RESET_WINDOW {
        t.restart_streak = 0;
        t.degraded = false;
    }
}

/// Best-effort panic payload extraction for the ERROR log line.
fn panic_message(err: tokio::task::JoinError) -> String {
    if !err.is_panic() {
        return "task cancelled".to_string();
    }
    let payload = err.into_panic();
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    }
}

/// `JoinSet`-based supervisor for the host's background loops. Build one
/// with [`Supervisor::new`], [`Supervisor::register`] each loop, then call
/// [`Supervisor::run`] — the returned handle completes once shutdown has
/// been requested AND every loop has drained.
pub struct Supervisor {
    status: Arc<SupervisorStatus>,
    names: Vec<&'static str>,
    factories: Vec<LoopFactory>,
    shutdown: CancellationToken,
}

impl std::fmt::Debug for Supervisor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Supervisor")
            .field("loops", &self.names)
            .finish_non_exhaustive()
    }
}

impl Supervisor {
    /// New supervisor bound to the host's shutdown token. The token is
    /// used ONLY to distinguish expected drains from unexpected exits and
    /// to abort pending backoff waits — the loops themselves keep watching
    /// the same token they always did.
    pub fn new(shutdown: CancellationToken) -> Self {
        Self {
            status: Arc::new(SupervisorStatus::new()),
            names: Vec::new(),
            factories: Vec::new(),
            shutdown,
        }
    }

    /// The shared status view (hand this to the admin-socket handler).
    pub fn status(&self) -> Arc<SupervisorStatus> {
        Arc::clone(&self.status)
    }

    /// Register a named loop. `factory` is called once when
    /// [`Supervisor::run`] starts and once per restart; each call must
    /// return a fresh future that runs the loop until shutdown.
    pub fn register<F, Fut>(&mut self, name: &'static str, mut factory: F)
    where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.status.add_task(name);
        self.names.push(name);
        self.factories.push(Box::new(move || Box::pin(factory())));
    }

    /// Spawn every registered loop and the driver task. The returned
    /// handle resolves after shutdown once every loop has drained —
    /// awaiting it replaces the per-handle awaits boot used to do.
    pub fn run(self) -> tokio::task::JoinHandle<()> {
        let Self {
            status,
            names: _,
            factories,
            shutdown,
        } = self;
        tokio::spawn(drive(factories, status, shutdown))
    }
}

/// The driver: spawns each loop into a [`JoinSet`], then services exits.
/// Unexpected exits (panic or return while the host is running) are logged
/// at ERROR and respawned after the backoff delay; exits after shutdown
/// are the normal drain and are simply recorded. The driver returns once
/// shutdown has been requested and the set is empty.
async fn drive(
    mut factories: Vec<LoopFactory>,
    status: Arc<SupervisorStatus>,
    shutdown: CancellationToken,
) {
    let mut set: JoinSet<usize> = JoinSet::new();
    let mut loop_by_task: HashMap<tokio::task::Id, usize> = HashMap::new();
    let now = Instant::now();
    for (idx, factory) in factories.iter_mut().enumerate() {
        let fut = (factory)();
        status.mark_started(idx, now);
        let handle = set.spawn(async move {
            fut.await;
            idx
        });
        loop_by_task.insert(handle.id(), idx);
    }

    while let Some(res) = set.join_next_with_id().await {
        let (idx, exit) = match res {
            Ok((id, idx)) => {
                loop_by_task.remove(&id);
                (idx, ExitKind::Returned)
            }
            Err(err) => {
                let id = err.id();
                let Some(idx) = loop_by_task.remove(&id) else {
                    // Unknown id — nothing we spawned; ignore defensively.
                    continue;
                };
                (idx, ExitKind::Panicked(panic_message(err)))
            }
        };
        let name = status.name(idx);
        if shutdown.is_cancelled() {
            status.mark_stopped(idx);
            info!(loop_name = name, "supervised loop drained on shutdown");
            continue;
        }

        // Unexpected exit while the host is running: the class of failure
        // that used to silently kill the subsystem. Log loud, back off,
        // respawn.
        let (delay, restarts, degraded) =
            status.record_unexpected_exit(idx, exit.describe(), Instant::now());
        // M21 S1 (M1 rider): monotonic restart count by loop + reason.
        copperclaw_metrics::inc_supervised_loop_restart(name, exit.metric_reason());
        error!(
            loop_name = name,
            reason = %exit.describe(),
            restarts,
            backoff_secs = delay.as_secs(),
            degraded,
            "supervised loop exited unexpectedly; restarting after backoff"
        );
        let fut = (factories[idx])();
        let sd = shutdown.clone();
        let st = Arc::clone(&status);
        let handle = set.spawn(async move {
            tokio::select! {
                () = sd.cancelled() => return idx,
                () = tokio::time::sleep(delay) => {}
            }
            st.mark_started(idx, Instant::now());
            fut.await;
            idx
        });
        loop_by_task.insert(handle.id(), idx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------
    // State-machine tests: exercise the backoff / degraded / healthy-reset
    // bookkeeping directly for exact, scheduling-independent semantics.
    // ------------------------------------------------------------------

    #[tokio::test(start_paused = true)]
    async fn backoff_walks_the_curve_and_flips_degraded_past_the_cap() {
        let status = SupervisorStatus::new();
        let idx = status.add_task("t");
        status.mark_started(idx, Instant::now());
        let mut observed = Vec::new();
        for _ in 0..6 {
            let (delay, _total, degraded) =
                status.record_unexpected_exit(idx, "boom".into(), Instant::now());
            observed.push((delay, degraded));
            status.mark_started(idx, Instant::now());
        }
        // Restarts 1-4 walk the curve; 5+ hold the cap AND are degraded.
        assert_eq!(
            observed,
            vec![
                (Duration::from_secs(5), false),
                (Duration::from_secs(15), false),
                (Duration::from_secs(60), false),
                (Duration::from_secs(300), false),
                (Duration::from_secs(300), true),
                (Duration::from_secs(300), true),
            ]
        );
        assert!(status.degraded(), "supervisor-wide flag follows the task");
    }

    #[tokio::test(start_paused = true)]
    async fn degraded_watch_observes_the_flip() {
        let status = SupervisorStatus::new();
        let idx = status.add_task("t");
        status.mark_started(idx, Instant::now());
        let mut rx = status.degraded_watch();
        assert!(!*rx.borrow());
        for _ in 0..4 {
            status.record_unexpected_exit(idx, "boom".into(), Instant::now());
            status.mark_started(idx, Instant::now());
            assert!(!rx.has_changed().unwrap(), "no flip within the curve");
        }
        status.record_unexpected_exit(idx, "boom".into(), Instant::now());
        assert!(rx.has_changed().unwrap(), "O4 seam sees the transition");
        assert!(*rx.borrow_and_update());
    }

    #[tokio::test(start_paused = true)]
    async fn ten_healthy_minutes_reset_the_streak_and_clear_degraded() {
        let status = SupervisorStatus::new();
        let idx = status.add_task("t");
        status.mark_started(idx, Instant::now());
        // Degrade: five instant crashes.
        for _ in 0..5 {
            status.record_unexpected_exit(idx, "boom".into(), Instant::now());
            status.mark_started(idx, Instant::now());
        }
        assert!(status.degraded());

        // The incarnation now runs healthy through the reset window; its
        // next crash starts a fresh streak on the FIRST backoff step.
        tokio::time::advance(HEALTHY_RESET_WINDOW).await;
        let (delay, total, degraded) =
            status.record_unexpected_exit(idx, "boom".into(), Instant::now());
        assert_eq!(delay, BACKOFF_STEPS[0], "curve starts over after healing");
        assert_eq!(total, 6, "lifetime total keeps the history");
        assert!(!degraded);
        assert!(!status.degraded());
        let snap = status.snapshot();
        assert_eq!(snap[0].restart_streak, 1);
        assert!(!snap[0].degraded);
    }

    #[tokio::test(start_paused = true)]
    async fn one_second_short_of_the_window_does_not_reset() {
        let status = SupervisorStatus::new();
        let idx = status.add_task("t");
        status.mark_started(idx, Instant::now());
        status.record_unexpected_exit(idx, "boom".into(), Instant::now());
        status.mark_started(idx, Instant::now());
        tokio::time::advance(HEALTHY_RESET_WINDOW - Duration::from_secs(1)).await;
        let (delay, _total, _degraded) =
            status.record_unexpected_exit(idx, "boom".into(), Instant::now());
        assert_eq!(delay, BACKOFF_STEPS[1], "streak continues below the window");
    }

    #[tokio::test(start_paused = true)]
    async fn snapshot_alone_heals_a_long_healthy_loop() {
        let status = SupervisorStatus::new();
        let idx = status.add_task("t");
        status.mark_started(idx, Instant::now());
        for _ in 0..5 {
            status.record_unexpected_exit(idx, "boom".into(), Instant::now());
            status.mark_started(idx, Instant::now());
        }
        assert!(status.degraded());

        // No further exits — the lazy heal in snapshot() clears the flag
        // once the running incarnation crosses the window.
        tokio::time::advance(HEALTHY_RESET_WINDOW).await;
        let snap = status.snapshot();
        assert!(!snap[0].degraded);
        assert_eq!(snap[0].restart_streak, 0);
        assert_eq!(snap[0].restarts, 5, "lifetime total survives the heal");
        assert!(!status.degraded(), "watch value cleared by snapshot heal");
        assert_eq!(snap[0].uptime_secs, HEALTHY_RESET_WINDOW.as_secs());
    }

    #[test]
    fn backoff_curve_matches_decision_e() {
        assert_eq!(
            BACKOFF_STEPS,
            [
                Duration::from_secs(5),
                Duration::from_secs(15),
                Duration::from_secs(60),
                Duration::from_secs(300),
            ]
        );
        assert_eq!(HEALTHY_RESET_WINDOW, Duration::from_secs(600));
    }

    // ------------------------------------------------------------------
    // Scheduling tests: run the real driver on a paused clock. Parking
    // auto-advances the clock to the next pending timer, so restart
    // intervals are asserted from instants recorded INSIDE each
    // incarnation — exact under the mock clock, no real waits.
    // ------------------------------------------------------------------

    /// Instants at which each incarnation actually began executing.
    type StartLog = Arc<Mutex<Vec<Instant>>>;

    /// A loop that panics for its first `panics` incarnations, then runs
    /// until cancelled. Start instants are recorded by the FUTURE (not the
    /// factory), so they reflect when the incarnation truly began.
    fn flaky_loop(
        log: StartLog,
        panics: usize,
        shutdown: CancellationToken,
    ) -> impl FnMut() -> LoopFuture {
        move || {
            let log = Arc::clone(&log);
            let sd = shutdown.clone();
            Box::pin(async move {
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(Instant::now());
                    l.len()
                };
                assert!(n > panics, "injected failure {n}");
                sd.cancelled().await;
            })
        }
    }

    fn starts(log: &StartLog) -> Vec<Instant> {
        log.lock().unwrap().clone()
    }

    /// Poll until `cond` holds. The short mock-clock sleep parks the
    /// runtime, and each park auto-advances the paused clock to the next
    /// pending timer — so backoff waits are traversed deterministically
    /// and every timed event fires at its exact deadline. Capped so a bug
    /// fails instead of hanging.
    async fn wait_until(mut cond: impl FnMut() -> bool) {
        for _ in 0..100_000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!("condition not reached under the paused clock");
    }

    #[tokio::test(start_paused = true)]
    async fn panicking_loop_restarts_on_the_backoff_curve() {
        let shutdown = CancellationToken::new();
        let mut sup = Supervisor::new(shutdown.clone());
        let log: StartLog = Arc::default();
        sup.register("flaky", flaky_loop(Arc::clone(&log), 2, shutdown.clone()));
        let status = sup.status();
        let driver = sup.run();

        wait_until(|| starts(&log).len() >= 3).await;
        let s = starts(&log);
        // Each panic is instantaneous, so the gap between incarnation
        // starts IS the backoff delay.
        assert_eq!(s[1] - s[0], BACKOFF_STEPS[0], "first restart after 5s");
        assert_eq!(s[2] - s[1], BACKOFF_STEPS[1], "second restart after 15s");

        wait_until(|| status.snapshot()[0].alive).await;
        let snap = status.snapshot();
        assert_eq!(snap[0].name, "flaky");
        assert_eq!(snap[0].restarts, 2);
        assert_eq!(snap[0].restart_streak, 2);
        assert!(!snap[0].degraded);
        assert!(
            snap[0]
                .last_exit
                .as_deref()
                .is_some_and(|e| e.contains("injected failure 2")),
            "last_exit should carry the panic message: {:?}",
            snap[0].last_exit
        );

        shutdown.cancel();
        driver.await.unwrap();
        assert!(!status.snapshot()[0].alive, "drained on shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn crash_looping_task_degrades_and_keeps_retrying_at_the_cap() {
        let shutdown = CancellationToken::new();
        let mut sup = Supervisor::new(shutdown.clone());
        let log: StartLog = Arc::default();
        // Panics forever: every incarnation dies instantly.
        sup.register(
            "doomed",
            flaky_loop(Arc::clone(&log), usize::MAX, shutdown.clone()),
        );
        let status = sup.status();
        let driver = sup.run();

        wait_until(|| starts(&log).len() >= 6).await;
        let s = starts(&log);
        assert_eq!(s[1] - s[0], BACKOFF_STEPS[0]);
        assert_eq!(s[2] - s[1], BACKOFF_STEPS[1]);
        assert_eq!(s[3] - s[2], BACKOFF_STEPS[2]);
        assert_eq!(s[4] - s[3], BACKOFF_STEPS[3]);
        assert_eq!(s[5] - s[4], BACKOFF_STEPS[3], "held at the cap");
        assert!(status.degraded(), "exceeding the cap flips degraded");
        let snap = status.snapshot();
        assert!(snap[0].degraded);
        assert!(snap[0].restarts >= 5);

        shutdown.cancel();
        driver.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn healthy_incarnation_resets_the_live_backoff_curve() {
        let shutdown = CancellationToken::new();
        let mut sup = Supervisor::new(shutdown.clone());
        let log: StartLog = Arc::default();
        let log_in_loop = Arc::clone(&log);
        let sd_in_loop = shutdown.clone();
        // Incarnations 1-2 panic instantly; incarnation 3 stays healthy
        // past the reset window, then panics; incarnation 4 runs until
        // cancelled.
        sup.register("recovering", move || {
            let log = Arc::clone(&log_in_loop);
            let sd = sd_in_loop.clone();
            Box::pin(async move {
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(Instant::now());
                    l.len()
                };
                match n {
                    1 | 2 => panic!("early crash {n}"),
                    3 => {
                        tokio::time::sleep(HEALTHY_RESET_WINDOW + Duration::from_secs(1)).await;
                        panic!("late crash");
                    }
                    _ => sd.cancelled().await,
                }
            }) as LoopFuture
        });
        let status = sup.status();
        let driver = sup.run();

        wait_until(|| starts(&log).len() >= 4).await;
        let s = starts(&log);
        assert_eq!(s[1] - s[0], BACKOFF_STEPS[0]);
        assert_eq!(s[2] - s[1], BACKOFF_STEPS[1]);
        // Incarnation 3 ran 601s (past the window) before its late crash,
        // so its restart pays only the FIRST backoff step again.
        assert_eq!(
            s[3] - s[2],
            HEALTHY_RESET_WINDOW + Duration::from_secs(1) + BACKOFF_STEPS[0],
            "post-heal restart uses the first backoff step"
        );
        wait_until(|| status.snapshot()[0].alive).await;
        let snap = status.snapshot();
        assert_eq!(snap[0].restart_streak, 1, "streak restarted from zero");
        assert_eq!(snap[0].restarts, 3, "lifetime total keeps the history");
        assert!(!snap[0].degraded);

        shutdown.cancel();
        driver.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn unexpected_return_is_also_restarted() {
        let shutdown = CancellationToken::new();
        let mut sup = Supervisor::new(shutdown.clone());
        let log: StartLog = Arc::default();
        let log_in_loop = Arc::clone(&log);
        let sd_in_loop = shutdown.clone();
        // First incarnation returns immediately (e.g. a closed inbound
        // channel); the second runs until cancelled.
        sup.register("returner", move || {
            let log = Arc::clone(&log_in_loop);
            let sd = sd_in_loop.clone();
            Box::pin(async move {
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(Instant::now());
                    l.len()
                };
                if n == 1 {
                    return;
                }
                sd.cancelled().await;
            }) as LoopFuture
        });
        let status = sup.status();
        let driver = sup.run();

        wait_until(|| starts(&log).len() >= 2).await;
        let s = starts(&log);
        assert_eq!(s[1] - s[0], BACKOFF_STEPS[0], "restarted after 5s");
        wait_until(|| status.snapshot()[0].alive).await;
        let snap = status.snapshot();
        assert_eq!(snap[0].restarts, 1);
        assert_eq!(snap[0].last_exit.as_deref(), Some("returned unexpectedly"));

        shutdown.cancel();
        driver.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_drains_all_loops_without_counting_restarts() {
        let shutdown = CancellationToken::new();
        let mut sup = Supervisor::new(shutdown.clone());
        for name in ["a", "b", "c"] {
            let sd = shutdown.clone();
            sup.register(name, move || {
                let sd = sd.clone();
                Box::pin(async move { sd.cancelled().await }) as LoopFuture
            });
        }
        let status = sup.status();
        let driver = sup.run();
        wait_until(|| status.snapshot().iter().all(|l| l.alive)).await;

        shutdown.cancel();
        driver.await.unwrap();
        let snap = status.snapshot();
        assert_eq!(snap.len(), 3);
        assert!(snap.iter().all(|l| !l.alive));
        assert!(snap.iter().all(|l| l.restarts == 0));
        assert!(!status.degraded());
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_during_backoff_drains_without_further_restarts() {
        let shutdown = CancellationToken::new();
        let mut sup = Supervisor::new(shutdown.clone());
        let log: StartLog = Arc::default();
        sup.register(
            "waiting",
            flaky_loop(Arc::clone(&log), usize::MAX, shutdown.clone()),
        );
        let status = sup.status();
        let driver = sup.run();
        // At least one panic has been recorded; a restart wrapper is (or
        // will be) waiting out a backoff.
        wait_until(|| status.snapshot()[0].restarts >= 1).await;
        shutdown.cancel();
        let restarts_at_cancel = status.snapshot()[0].restarts;
        driver.await.unwrap();
        let snap = status.snapshot();
        assert!(!snap[0].alive, "drained");
        assert_eq!(
            snap[0].restarts, restarts_at_cancel,
            "no restart counted after cancel"
        );
    }
}
