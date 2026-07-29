//! Background ticker that keeps the "agent is working" typing indicator
//! visible on each channel-connected session.
//!
//! Why this exists: most channel `set_typing` APIs (Telegram's
//! `sendChatAction`, Slack's assistant `typing`, etc.) only display the
//! indicator for ~5 seconds per call. The `TypingModule`'s
//! rate-limited `set_typing` fires when an inbound event arrives, but
//! during long agent turns (LLM call + tool dispatch loop) no inbound
//! is firing, so the bubble fades and the user thinks the bot is
//! hung. This task runs alongside the delivery + sweep loops and
//! re-fires `set_typing` every [`TICK_INTERVAL`] on every session
//! whose container is currently `Running` AND has a messaging-group
//! routing (so the typing indicator has somewhere to land).
//!
//! Idle / Stopped sessions are skipped — typing while nothing is
//! actively processing would be a lie. The host's container-manager
//! state transitions (`mark_container_running` / `_idle` / `_stopped`)
//! are the source of truth.
//!
//! One deliberate widening (M21 F1, decision (c)): sessions that are
//! **mid-spawn** with pending inbound also pulse typing. A first
//! message to a fresh session used to get zero feedback for the entire
//! container spawn (image build, boot, runner handshake); with the
//! container manager's [`SpawnActivity`] registry wired in (see
//! [`TypingTicker::with_spawn_activity`]), typing shows within one tick
//! of the message landing — before the runner is up. Without the
//! registry (the default, and every pre-F1 test), behavior for
//! `Running` sessions is byte-identical to before.

use crate::container_manager::SpawnActivity;
use copperclaw_db::central::CentralDb;
use copperclaw_db::session::{SessionPaths, open_inbound_ro_no_mmap};
use copperclaw_db::tables::{messages_in, messaging_groups, sessions};
use copperclaw_modules::{DeliveryDispatcher, DispatchTarget, TypingOutcome};
use copperclaw_types::{Session, SessionId};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// How often to re-fire `set_typing` per running session. 4s matches
/// `TypingModule::DEFAULT_INTERVAL_MS` and is below Telegram's ~5s
/// indicator-fade-out so the bubble stays solid.
pub const TICK_INTERVAL: Duration = Duration::from_secs(4);

/// Fallback cooldown applied when a channel rate-limits `set_typing` but
/// gives no `retry_after`. Ten seconds is comfortably past Telegram's
/// chat-action window without stalling the indicator for a whole turn.
const RATE_LIMIT_FALLBACK: Duration = Duration::from_secs(10);

/// Service that keeps the typing indicator alive during long agent
/// turns. Spawn one per host via [`run_loop`].
pub struct TypingTicker {
    central: CentralDb,
    dispatcher: Arc<dyn DeliveryDispatcher>,
    data_root: PathBuf,
    interval: Duration,
    /// Per-session "last seen pending=true" timestamps. Avoids reopening
    /// the per-session `inbound.db` sqlite handle every tick for a
    /// continuously-busy session: once we've confirmed work-in-flight
    /// for a session, subsequent ticks within `cache_window()` short-
    /// circuit to "yes, still busy" without touching disk. When a
    /// session goes idle (count drops to 0), the entry is evicted, so
    /// the next pending-work transition reopens once and re-primes the
    /// cache. Steady-state sqlite churn drops from O(running sessions
    /// per tick) to O(idle transitions per tick).
    last_seen_pending: RwLock<HashMap<SessionId, Instant>>,
    /// Per-session rate-limit cooldown: the `Instant` until which we must
    /// NOT re-fire `set_typing` for this session. Populated when a prior
    /// dispatch reports [`TypingOutcome::RateLimited`] (Telegram et al.
    /// answer `set_typing` with `Rate { retry_after }` and hammering the
    /// next tick just earns more 429s + warn spam). Evicted alongside
    /// `last_seen_pending` when the session goes idle.
    cooldowns: RwLock<HashMap<SessionId, Instant>>,
    /// In-flight typing-dispatch outcome receivers, keyed by session. The
    /// dispatcher runs the adapter call on a spawned task, so the outcome
    /// (ok / rate-limited) arrives asynchronously; we stash the receiver
    /// here and drain it (non-blocking) at the head of the next tick to
    /// apply any cooldown. One in-flight receipt per session is enough —
    /// a newer dispatch supersedes the last.
    pending_receipts: Mutex<HashMap<SessionId, oneshot::Receiver<TypingOutcome>>>,
    /// M21 F1 (decision (c)): the container manager's registry of spawn
    /// attempts currently in flight. When wired (boot hands the same
    /// handle to the manager), sessions mid-spawn with pending inbound
    /// pulse typing too, so a cold start shows life from message one.
    /// `None` (the default) keeps the `Running`-only gate exactly as it
    /// was.
    spawn_activity: Option<Arc<SpawnActivity>>,
}

impl TypingTicker {
    pub fn new(
        central: CentralDb,
        dispatcher: Arc<dyn DeliveryDispatcher>,
        data_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            central,
            dispatcher,
            data_root: data_root.into(),
            interval: TICK_INTERVAL,
            last_seen_pending: RwLock::new(HashMap::new()),
            cooldowns: RwLock::new(HashMap::new()),
            pending_receipts: Mutex::new(HashMap::new()),
            spawn_activity: None,
        }
    }

    /// Wire the container manager's spawn-activity registry so the
    /// ticker also covers sessions mid-spawn (M21 F1). See the module
    /// docs for the widened-gate rationale.
    #[must_use]
    pub fn with_spawn_activity(mut self, activity: Arc<SpawnActivity>) -> Self {
        self.spawn_activity = Some(activity);
        self
    }

    /// How long a "pending=true" observation stays trusted before we
    /// reopen sqlite. Two tick intervals — long enough to skip the
    /// sqlite roundtrip on the next tick of a continuously-busy
    /// session, short enough that an idle-going session reopens within
    /// one extra tick.
    fn cache_window(&self) -> Duration {
        self.interval * 2
    }

    /// Test-seam: override the tick interval.
    #[cfg(test)]
    #[must_use]
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Drive one pass of the ticker — for every running session with a
    /// channel-bound messaging group (plus, when a [`SpawnActivity`]
    /// registry is wired, every session mid-spawn), fire a `set_typing`
    /// through the dispatcher. Exposed `pub(crate)` so the loop and
    /// tests can call it directly; the public surface is [`run_loop`].
    pub(crate) fn tick(&self) -> usize {
        // Apply any rate-limit feedback from the previous pass's dispatches
        // before deciding who to ping this time.
        self.drain_typing_feedback();
        let running = match sessions::list_running(&self.central) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "typing_ticker: list_running failed; skipping pass");
                return 0;
            }
        };
        let now = Instant::now();
        let mut fired = 0usize;
        let running_ids: std::collections::HashSet<SessionId> =
            running.iter().map(|s| s.id).collect();
        for s in &running {
            if self.try_fire(s, now) {
                fired += 1;
            }
        }
        // M21 F1 (decision (c)): sessions whose container spawn is
        // currently in flight get the same treatment — a cold start
        // shows typing from message one instead of dead air for the
        // whole image-build/boot/handshake window. The registry only
        // ever holds sessions past the manager's pending-inbound,
        // budget, and rate-limit gates, and every dispatch below still
        // passes the identical pending-inbound / cooldown / routing
        // gates as the `Running` arm. Sessions already covered above
        // are skipped so a spawn racing `mark_container_running`
        // can't double-fire.
        if let Some(activity) = &self.spawn_activity {
            for session_id in activity.active_sessions() {
                if running_ids.contains(&session_id) {
                    continue;
                }
                let s = match sessions::get(&self.central, session_id) {
                    Ok(s) => s,
                    Err(err) => {
                        debug!(
                            ?err,
                            session = %session_id.as_uuid(),
                            "typing_ticker: sessions::get failed for mid-spawn session; skipping",
                        );
                        continue;
                    }
                };
                if self.try_fire(&s, now) {
                    fired += 1;
                }
            }
        }
        fired
    }

    /// Gate + dispatch one session's typing pulse. Shared by the
    /// `Running` arm and the mid-spawn arm of [`Self::tick`]; the check
    /// order (messaging-group routing, pending inbound, rate-limit
    /// cooldown, group lookup) is exactly the pre-F1 `Running`-arm
    /// sequence. Returns whether a dispatch was fired.
    fn try_fire(&self, s: &Session, now: Instant) -> bool {
        let Some(mg_id) = s.messaging_group_id else {
            return false;
        };
        // Gate on actual work-in-flight, not just container=Running:
        // a session that's been Running for the whole idle-timeout
        // window between user turns should NOT pulse typing
        // continuously. Check via `has_pending_inbound`, which is
        // backed by a short-lived per-session cache so we don't
        // reopen sqlite on every tick of a continuously-busy
        // session. If > 0, the agent has work it's about to
        // process (or is processing); fire typing. If 0, stay
        // quiet.
        if !self.has_pending_inbound(s.agent_group_id, s.id) {
            return false;
        }
        // Respect a live rate-limit cooldown: a prior tick's dispatch
        // came back `RateLimited`, so stay quiet for this target until
        // the backoff elapses instead of hammering the adapter again.
        if self.in_cooldown(s.id, now) {
            return false;
        }
        let mg = match messaging_groups::get(&self.central, mg_id) {
            Ok(m) => m,
            Err(err) => {
                debug!(
                    ?err,
                    session = %s.id.as_uuid(),
                    "typing_ticker: messaging_groups::get failed; skipping",
                );
                return false;
            }
        };
        let target = DispatchTarget::channel(mg.channel_type, mg.platform_id, s.thread_id.clone());
        if let Some(rx) = self.dispatcher.set_typing(&target) {
            // Stash the outcome receiver so the next tick can apply any
            // cooldown; a fresh dispatch supersedes an earlier pending one.
            if let Ok(mut guard) = self.pending_receipts.lock() {
                guard.insert(s.id, rx);
            }
        }
        true
    }

    /// Drain the outcome receivers stashed by the previous tick's
    /// dispatches, applying a per-session cooldown for any that came back
    /// [`TypingOutcome::RateLimited`]. Non-blocking: receivers still in
    /// flight (adapter call not yet resolved) are kept for a later drain;
    /// resolved / closed ones are removed.
    fn drain_typing_feedback(&self) {
        let Ok(mut receipts) = self.pending_receipts.lock() else {
            return;
        };
        let now = Instant::now();
        receipts.retain(|session_id, rx| match rx.try_recv() {
            Ok(TypingOutcome::RateLimited { retry_after }) => {
                let backoff = retry_after.map_or(RATE_LIMIT_FALLBACK, Duration::from_secs);
                if let Ok(mut cooldowns) = self.cooldowns.write() {
                    cooldowns.insert(*session_id, now + backoff);
                }
                false
            }
            // Success or a dropped sender (nothing dispatched): no cooldown,
            // drop the receipt.
            Ok(TypingOutcome::Ok) | Err(oneshot::error::TryRecvError::Closed) => false,
            // Adapter call hasn't resolved yet — keep waiting.
            Err(oneshot::error::TryRecvError::Empty) => true,
        });
    }

    /// Whether `session_id` is inside a live rate-limit cooldown as of `now`.
    fn in_cooldown(&self, session_id: SessionId, now: Instant) -> bool {
        self.cooldowns
            .read()
            .ok()
            .and_then(|g| g.get(&session_id).copied())
            .is_some_and(|until| now < until)
    }

    /// Cheaply check whether a session has unprocessed inbound rows.
    ///
    /// First consults a per-session in-memory cache: if the session
    /// last reported pending work within `cache_window()`, return true
    /// without reopening sqlite. Otherwise open inbound.db read-only
    /// (host writes inbound, so a RO handle here is safe), count any
    /// pending row whose `process_after` is null or due — *without*
    /// the trigger=1 filter, because the runner's first-poll pass
    /// picks up non-trigger rows (agent-to-agent dispatch, scheduled
    /// Task/wake messages, system messages) too and the typing
    /// indicator should stay alive during those turns.
    ///
    /// On a successful count, the cache is primed (pending) or evicted
    /// (idle). On a DB-open or count error, we log at debug (these are
    /// expected transient cases — brand-new session DB not fully
    /// initialised, momentary lock contention) and fall through to
    /// false; the next tick retries.
    fn has_pending_inbound(
        &self,
        agent_group_id: copperclaw_types::AgentGroupId,
        session_id: SessionId,
    ) -> bool {
        // Fast path: cached "yes" within the window.
        if let Ok(guard) = self.last_seen_pending.read() {
            if let Some(seen) = guard.get(&session_id) {
                if seen.elapsed() < self.cache_window() {
                    return true;
                }
            }
        }
        let paths = SessionPaths::new(&self.data_root, agent_group_id, session_id);
        let conn = match open_inbound_ro_no_mmap(&paths) {
            Ok(c) => c,
            Err(err) => {
                // Don't swallow silently: an operator chasing "why
                // isn't the typing indicator on?" otherwise has no
                // signal. Debug-level — these are expected during
                // session bring-up — and we still return false so
                // the ticker stays quiet for this pass.
                debug!(
                    ?err,
                    session = %session_id.as_uuid(),
                    "typing-ticker: could not open inbound.db; treating as no pending work",
                );
                return false;
            }
        };
        match messages_in::count_pending_for_typing(&conn) {
            Ok(n) if n > 0 => {
                if let Ok(mut guard) = self.last_seen_pending.write() {
                    guard.insert(session_id, Instant::now());
                }
                true
            }
            Ok(_) => {
                // Idle: drop any stale cache entry so the next
                // pending-work transition reopens sqlite cleanly. Evict the
                // rate-limit cooldown + any pending typing receipt in the
                // same breath — an idle session carries no live typing state.
                if let Ok(mut guard) = self.last_seen_pending.write() {
                    guard.remove(&session_id);
                }
                if let Ok(mut guard) = self.cooldowns.write() {
                    guard.remove(&session_id);
                }
                if let Ok(mut guard) = self.pending_receipts.lock() {
                    guard.remove(&session_id);
                }
                false
            }
            Err(err) => {
                debug!(
                    ?err,
                    session = %session_id.as_uuid(),
                    "typing-ticker: count_pending_for_typing failed; treating as no pending work",
                );
                false
            }
        }
    }
}

impl TypingTicker {
    /// Loop until `shutdown` is cancelled, firing one [`tick`] per
    /// `interval`. The shutdown branch wins — when the cancel token
    /// fires mid-sleep the loop drops out promptly.
    ///
    /// [`tick`]: Self::tick
    pub async fn run_loop(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(self.interval) => {
                    let _ = self.tick();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use copperclaw_db::tables::{
        agent_groups::{self, CreateAgentGroup},
        messaging_groups::UpsertMessagingGroup,
        sessions::CreateSession,
    };
    use copperclaw_types::ChannelType;
    use std::sync::{Arc as StdArc, Mutex};

    /// Captures every dispatcher call so tests can assert on the
    /// `set_typing` invocation count + targets. When `next_outcome` is set,
    /// every `set_typing` reports that outcome back through the returned
    /// receiver so cooldown behaviour can be driven.
    #[derive(Default)]
    struct MockDispatcher {
        typing_calls: Mutex<Vec<DispatchTarget>>,
        next_outcome: Mutex<Option<TypingOutcome>>,
    }

    impl MockDispatcher {
        /// Make every subsequent `set_typing` come back rate-limited with
        /// the given `retry_after` (seconds).
        fn rate_limit(&self, retry_after: Option<u64>) {
            *self.next_outcome.lock().unwrap() = Some(TypingOutcome::RateLimited { retry_after });
        }
    }

    impl DeliveryDispatcher for MockDispatcher {
        fn set_typing(&self, target: &DispatchTarget) -> Option<oneshot::Receiver<TypingOutcome>> {
            self.typing_calls.lock().unwrap().push(target.clone());
            let outcome = self.next_outcome.lock().unwrap().clone();
            outcome.map(|o| {
                let (tx, rx) = oneshot::channel();
                // Resolve immediately so the next tick's drain sees it.
                let _ = tx.send(o);
                rx
            })
        }
        fn dispatch(&self, _target: &DispatchTarget, _message: &copperclaw_types::OutboundMessage) {
            // Not used by the ticker; assert at test sites if they
            // expect dispatch.
        }
    }

    fn fresh_central() -> (tempfile::TempDir, CentralDb) {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open(tmp.path().join("c.db").as_path()).unwrap();
        (tmp, db)
    }

    /// Build a running session AND seed its inbound.db with one pending
    /// chat row so `has_pending_inbound` returns true (the ticker now
    /// gates on actual work-in-flight). `data_root` should be the
    /// tempdir from `fresh_central()` so the session-dir layout matches
    /// production.
    fn make_running_session_with_pending(
        central: &CentralDb,
        data_root: &std::path::Path,
        ch: &str,
    ) -> copperclaw_types::SessionId {
        let g = agent_groups::create(
            central,
            CreateAgentGroup {
                name: ch.into(),
                folder: ch.into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new(ch),
                platform_id: format!("chat-{ch}"),
                name: Some("test".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let s = sessions::create(
            central,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: Some(mg.id),
                thread_id: None,
                agent_provider: None,
                source_session_id: None,
            },
        )
        .unwrap();
        sessions::mark_container_running(central, s.id).unwrap();
        // Seed a pending inbound row so the work-in-flight gate
        // returns true.
        let paths = copperclaw_db::session::SessionPaths::new(data_root, g.id, s.id);
        paths.ensure_dirs().unwrap();
        let conn = copperclaw_db::session::open_inbound(&paths).unwrap();
        copperclaw_db::tables::messages_in::insert(
            &conn,
            &copperclaw_db::tables::messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({ "text": "hi" }),
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
        s.id
    }

    #[test]
    fn tick_fires_set_typing_for_each_running_session_with_pending_work() {
        let (tmp, central) = fresh_central();
        let _s1 = make_running_session_with_pending(&central, tmp.path(), "telegram");
        let _s2 = make_running_session_with_pending(&central, tmp.path(), "slack");

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        let fired = ticker.tick();
        assert_eq!(
            fired, 2,
            "both running sessions with pending work should fire"
        );
        let calls = mock.typing_calls.lock().unwrap();
        let kinds: Vec<&str> = calls
            .iter()
            .map(|t| t.channel_type.as_ref().unwrap().as_str())
            .collect();
        assert!(kinds.contains(&"telegram"));
        assert!(kinds.contains(&"slack"));
    }

    #[test]
    fn tick_skips_idle_running_session_without_pending_work() {
        // A session whose container is `Running` but has no pending
        // inbound rows is between turns — typing here would be a lie.
        let (tmp, central) = fresh_central();
        // Build a session WITHOUT seeding a pending inbound row.
        let g = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "idle".into(),
                folder: "idle".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            &central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-idle".into(),
                name: Some("idle".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let s = sessions::create(
            &central,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: Some(mg.id),
                ..Default::default()
            },
        )
        .unwrap();
        sessions::mark_container_running(&central, s.id).unwrap();

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        assert_eq!(
            ticker.tick(),
            0,
            "Running session without pending inbound must NOT pulse typing",
        );
    }

    #[test]
    fn tick_skips_container_idle_sessions() {
        // Even with pending work, a session whose container_status is
        // not `Running` is excluded (the runner isn't on to process it).
        let (tmp, central) = fresh_central();
        let s = make_running_session_with_pending(&central, tmp.path(), "telegram");
        sessions::mark_container_idle(&central, s).unwrap();

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        assert_eq!(ticker.tick(), 0);
    }

    #[test]
    fn tick_skips_sessions_without_messaging_group() {
        let (tmp, central) = fresh_central();
        let g = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "lonely".into(),
                folder: "lonely".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let s = sessions::create(
            &central,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: None,
                ..Default::default()
            },
        )
        .unwrap();
        sessions::mark_container_running(&central, s.id).unwrap();

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        assert_eq!(ticker.tick(), 0);
    }

    /// Paused clock on purpose: the runtime advances time only once every
    /// task is parked, so 75ms of virtual time against a 20ms interval is
    /// exactly three ticks. Against the real clock this asserted "however
    /// many ticks a runner managed in 75ms of wall time", which a loaded
    /// macOS CI box lost (2 of 3).
    #[tokio::test(start_paused = true)]
    async fn run_loop_fires_repeatedly_until_shutdown() {
        let (tmp, central) = fresh_central();
        let _s = make_running_session_with_pending(&central, tmp.path(), "telegram");

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = Arc::new(
            TypingTicker::new(
                central,
                StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
                tmp.path(),
            )
            .with_interval(Duration::from_millis(20)),
        );
        let cancel = CancellationToken::new();
        let task = tokio::spawn(Arc::clone(&ticker).run_loop(cancel.clone()));

        // Ticks land at 20ms, 40ms and 60ms; the loop is parked until 80ms
        // when this wakes at 75ms and cancels.
        tokio::time::sleep(Duration::from_millis(75)).await;
        cancel.cancel();
        task.await.unwrap();

        let n = mock.typing_calls.lock().unwrap().len();
        assert_eq!(
            n, 3,
            "expected exactly 3 ticks in 75ms at a 20ms interval, got {n}"
        );
    }

    #[test]
    fn tick_counts_non_trigger_pending_for_typing() {
        // Finding #6 regression test: a session with only `trigger=false`
        // pending rows (agent-to-agent dispatch, scheduled wakes, system
        // messages) must still pulse typing — the runner picks them up
        // on its next poll and the indicator has to stay alive while
        // it does.
        let (tmp, central) = fresh_central();
        // Build a running session WITHOUT seeding a row via the helper;
        // we want trigger=false explicitly.
        let g = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "a2a".into(),
                folder: "a2a".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            &central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-a2a".into(),
                name: Some("a2a".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let s = sessions::create(
            &central,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: Some(mg.id),
                ..Default::default()
            },
        )
        .unwrap();
        sessions::mark_container_running(&central, s.id).unwrap();
        let paths = copperclaw_db::session::SessionPaths::new(tmp.path(), g.id, s.id);
        paths.ensure_dirs().unwrap();
        let conn = copperclaw_db::session::open_inbound(&paths).unwrap();
        copperclaw_db::tables::messages_in::insert(
            &conn,
            &copperclaw_db::tables::messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Agent,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({ "text": "from-other-agent" }),
                trigger: false, // <-- the case finding #6 was missing
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

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        assert_eq!(
            ticker.tick(),
            1,
            "trigger=false pending row must keep typing alive",
        );
    }

    #[test]
    fn has_pending_inbound_caches_busy_session() {
        // Finding #13: while a session is continuously busy, the cache
        // should short-circuit the sqlite reopen on subsequent ticks.
        // We can't directly observe "did sqlite open?" without surgery,
        // but we can: (a) seed a pending row, (b) call once to prime
        // the cache, (c) delete the row from sqlite, (d) call again
        // and assert the call still returns true (proof the cache is
        // being trusted), (e) trip the cache window by sleeping past
        // it and assert the call returns false (proof eviction + a
        // fresh read happens).
        let (tmp, central) = fresh_central();
        let s_id = make_running_session_with_pending(&central, tmp.path(), "telegram");
        let g_id = sessions::list_running(&central).unwrap()[0].agent_group_id;
        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        )
        .with_interval(Duration::from_millis(20)); // cache_window = 40ms

        // Prime the cache.
        assert!(ticker.has_pending_inbound(g_id, s_id));
        assert!(
            ticker.last_seen_pending.read().unwrap().contains_key(&s_id),
            "cache must record the busy session",
        );

        // Wipe the pending row behind sqlite's back. If the cache is
        // honoured, has_pending_inbound still returns true; if it's
        // not, it reopens, sees 0, and returns false.
        let paths = copperclaw_db::session::SessionPaths::new(tmp.path(), g_id, s_id);
        let conn = copperclaw_db::session::open_inbound(&paths).unwrap();
        conn.execute("DELETE FROM messages_in", []).unwrap();
        drop(conn);

        assert!(
            ticker.has_pending_inbound(g_id, s_id),
            "within the cache window, has_pending_inbound must short-circuit to true",
        );

        // Trip the cache window — cache_window = 2 * 20ms = 40ms.
        std::thread::sleep(Duration::from_millis(60));
        assert!(
            !ticker.has_pending_inbound(g_id, s_id),
            "after the cache window, a real sqlite read must surface the now-empty state",
        );
        assert!(
            !ticker.last_seen_pending.read().unwrap().contains_key(&s_id),
            "idle session must evict its cache entry",
        );
    }

    #[test]
    fn has_pending_inbound_evicts_on_idle() {
        // Finding #13 (eviction half): a session that reports 0 pending
        // must NOT linger in the cache — otherwise the next tick would
        // skip its sqlite read forever.
        let (tmp, central) = fresh_central();
        // Build a running session, no pending row.
        let g = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "idle-evict".into(),
                folder: "idle-evict".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            &central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-idle-evict".into(),
                name: Some("idle".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let s = sessions::create(
            &central,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: Some(mg.id),
                ..Default::default()
            },
        )
        .unwrap();
        sessions::mark_container_running(&central, s.id).unwrap();
        // Build the inbound DB file (empty).
        let paths = copperclaw_db::session::SessionPaths::new(tmp.path(), g.id, s.id);
        paths.ensure_dirs().unwrap();
        let _conn = copperclaw_db::session::open_inbound(&paths).unwrap();

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        assert!(!ticker.has_pending_inbound(g.id, s.id));
        assert!(
            !ticker.last_seen_pending.read().unwrap().contains_key(&s.id),
            "idle session must not be cached",
        );
    }

    #[test]
    fn rate_limited_dispatch_backs_off_then_resumes() {
        // The fix: when a set_typing dispatch comes back RateLimited, the
        // ticker must skip that session until the retry_after cooldown
        // elapses (instead of hammering the adapter every tick and
        // generating the observed warn spam), then resume.
        let (tmp, central) = fresh_central();
        let _s = make_running_session_with_pending(&central, tmp.path(), "telegram");
        let mock = StdArc::new(MockDispatcher::default());
        // 1s retry_after keeps the sleep short while still exercising the
        // whole-seconds cooldown path.
        mock.rate_limit(Some(1));
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );

        // Tick 1: fires (no cooldown yet); the mock hands back a
        // RateLimited receipt that resolves immediately.
        assert_eq!(ticker.tick(), 1, "first tick must fire");
        // Tick 2: drains the receipt, sets the cooldown, then skips.
        assert_eq!(ticker.tick(), 0, "tick within cooldown must NOT dispatch");
        // Tick 3 (still inside the 1s window): still skipped.
        assert_eq!(
            ticker.tick(),
            0,
            "second tick within cooldown must NOT dispatch"
        );
        assert_eq!(
            mock.typing_calls.lock().unwrap().len(),
            1,
            "no extra adapter calls while rate-limited",
        );

        // Let the cooldown lapse; the next tick must resume.
        std::thread::sleep(Duration::from_millis(1100));
        assert_eq!(
            ticker.tick(),
            1,
            "tick after the cooldown must dispatch again"
        );
        assert_eq!(
            mock.typing_calls.lock().unwrap().len(),
            2,
            "exactly one more adapter call after the cooldown lapsed",
        );
    }

    /// M21 F1: a stopped session whose spawn attempt is registered in
    /// the [`SpawnActivity`] registry pulses typing (pending inbound is
    /// present), and stops pulsing the moment the attempt unregisters.
    #[tokio::test]
    async fn tick_fires_for_mid_spawn_session_and_stops_when_attempt_ends() {
        let (tmp, central) = fresh_central();
        let s_id = make_running_session_with_pending(&central, tmp.path(), "telegram");
        // The cold-start shape: container Stopped, inbound pending.
        sessions::mark_container_stopped(&central, s_id).unwrap();

        let activity = StdArc::new(SpawnActivity::new());
        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        )
        .with_spawn_activity(StdArc::clone(&activity));

        // No attempt registered yet: a stopped session stays quiet.
        assert_eq!(
            ticker.tick(),
            0,
            "stopped session without a spawn in flight"
        );

        let generation = activity.begin(s_id);
        assert_eq!(ticker.tick(), 1, "mid-spawn session must pulse typing");
        assert_eq!(
            mock.typing_calls.lock().unwrap()[0]
                .channel_type
                .as_ref()
                .unwrap()
                .as_str(),
            "telegram",
        );

        activity.finish(s_id, generation);
        assert_eq!(ticker.tick(), 0, "typing stops when the attempt ends");
    }

    /// M21 F1: the mid-spawn arm keeps the work-in-flight gate — a
    /// registered attempt for a session with no pending inbound must
    /// not pulse (typing would be a lie).
    #[test]
    fn mid_spawn_session_without_pending_inbound_stays_quiet() {
        let (tmp, central) = fresh_central();
        let g = agent_groups::create(
            &central,
            CreateAgentGroup {
                name: "spawn-idle".into(),
                folder: "spawn-idle".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            &central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-spawn-idle".into(),
                name: Some("spawn-idle".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let s = sessions::create(
            &central,
            CreateSession {
                agent_group_id: g.id,
                messaging_group_id: Some(mg.id),
                ..Default::default()
            },
        )
        .unwrap();
        // Empty inbound.db: no pending work.
        let paths = copperclaw_db::session::SessionPaths::new(tmp.path(), g.id, s.id);
        paths.ensure_dirs().unwrap();
        let _conn = copperclaw_db::session::open_inbound(&paths).unwrap();

        let activity = StdArc::new(SpawnActivity::new());
        let _generation = activity.begin(s.id);
        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        )
        .with_spawn_activity(activity);
        assert_eq!(
            ticker.tick(),
            0,
            "mid-spawn session without pending inbound must NOT pulse typing",
        );
    }

    /// M21 F1: a session that is both `Running` and (racily) still in
    /// the spawn registry fires exactly once per tick, not twice.
    #[test]
    fn running_session_also_in_spawn_registry_fires_once() {
        let (tmp, central) = fresh_central();
        let s_id = make_running_session_with_pending(&central, tmp.path(), "telegram");
        let activity = StdArc::new(SpawnActivity::new());
        let _generation = activity.begin(s_id);
        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        )
        .with_spawn_activity(activity);
        assert_eq!(ticker.tick(), 1, "overlap must not double-fire");
        assert_eq!(mock.typing_calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn successful_dispatch_keeps_normal_cadence() {
        // Successful pings (no rate-limit feedback) must never install a
        // cooldown — every tick keeps firing at the usual cadence.
        let (tmp, central) = fresh_central();
        let _s = make_running_session_with_pending(&central, tmp.path(), "telegram");
        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        );
        for _ in 0..4 {
            assert_eq!(ticker.tick(), 1, "successful dispatch must fire every tick");
        }
        assert!(
            ticker.cooldowns.read().unwrap().is_empty(),
            "success must not install any cooldown",
        );
        assert_eq!(mock.typing_calls.lock().unwrap().len(), 4);
    }
}
