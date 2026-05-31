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

use ironclaw_db::central::CentralDb;
use ironclaw_db::session::{open_inbound_ro_no_mmap, SessionPaths};
use ironclaw_db::tables::{messages_in, messaging_groups, sessions};
use ironclaw_modules::{DeliveryDispatcher, DispatchTarget};
use ironclaw_types::SessionId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

/// How often to re-fire `set_typing` per running session. 4s matches
/// `TypingModule::DEFAULT_INTERVAL_MS` and is below Telegram's ~5s
/// indicator-fade-out so the bubble stays solid.
pub const TICK_INTERVAL: Duration = Duration::from_secs(4);

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
        }
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
    /// channel-bound messaging group, fire a `set_typing` through the
    /// dispatcher. Exposed `pub(crate)` so the loop and tests can call
    /// it directly; the public surface is [`run_loop`].
    pub(crate) fn tick(&self) -> usize {
        let running = match sessions::list_running(&self.central) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "typing_ticker: list_running failed; skipping pass");
                return 0;
            }
        };
        let mut fired = 0usize;
        for s in running {
            let Some(mg_id) = s.messaging_group_id else {
                continue;
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
                continue;
            }
            let mg = match messaging_groups::get(&self.central, mg_id) {
                Ok(m) => m,
                Err(err) => {
                    debug!(
                        ?err,
                        session = %s.id.as_uuid(),
                        "typing_ticker: messaging_groups::get failed; skipping",
                    );
                    continue;
                }
            };
            let target = DispatchTarget::channel(mg.channel_type, mg.platform_id, s.thread_id);
            self.dispatcher.set_typing(&target);
            fired += 1;
        }
        fired
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
        agent_group_id: ironclaw_types::AgentGroupId,
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
                // pending-work transition reopens sqlite cleanly.
                if let Ok(mut guard) = self.last_seen_pending.write() {
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
    use ironclaw_db::tables::{
        agent_groups::{self, CreateAgentGroup},
        messaging_groups::UpsertMessagingGroup,
        sessions::CreateSession,
    };
    use ironclaw_types::ChannelType;
    use std::sync::{Arc as StdArc, Mutex};

    /// Captures every dispatcher call so tests can assert on the
    /// `set_typing` invocation count + targets.
    #[derive(Default)]
    struct MockDispatcher {
        typing_calls: Mutex<Vec<DispatchTarget>>,
    }

    impl DeliveryDispatcher for MockDispatcher {
        fn set_typing(&self, target: &DispatchTarget) {
            self.typing_calls.lock().unwrap().push(target.clone());
        }
        fn dispatch(
            &self,
            _target: &DispatchTarget,
            _message: &ironclaw_types::OutboundMessage,
        ) {
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
    ) -> ironclaw_types::SessionId {
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
        let paths = ironclaw_db::session::SessionPaths::new(data_root, g.id, s.id);
        paths.ensure_dirs().unwrap();
        let conn = ironclaw_db::session::open_inbound(&paths).unwrap();
        ironclaw_db::tables::messages_in::insert(
            &conn,
            &ironclaw_db::tables::messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Chat,
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
        assert_eq!(fired, 2, "both running sessions with pending work should fire");
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

    #[tokio::test]
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

        // ~3 ticks in 75ms.
        tokio::time::sleep(Duration::from_millis(75)).await;
        cancel.cancel();
        task.await.unwrap();

        let n = mock.typing_calls.lock().unwrap().len();
        assert!(n >= 3, "expected at least 3 ticks, got {n}");
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
        let paths = ironclaw_db::session::SessionPaths::new(tmp.path(), g.id, s.id);
        paths.ensure_dirs().unwrap();
        let conn = ironclaw_db::session::open_inbound(&paths).unwrap();
        ironclaw_db::tables::messages_in::insert(
            &conn,
            &ironclaw_db::tables::messages_in::WriteInbound {
                id: ironclaw_types::MessageId::new(),
                kind: ironclaw_types::MessageKind::Agent,
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
        let paths = ironclaw_db::session::SessionPaths::new(tmp.path(), g_id, s_id);
        let conn = ironclaw_db::session::open_inbound(&paths).unwrap();
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
        let paths = ironclaw_db::session::SessionPaths::new(tmp.path(), g.id, s.id);
        paths.ensure_dirs().unwrap();
        let _conn = ironclaw_db::session::open_inbound(&paths).unwrap();

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
}
