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
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
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
        }
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
            // continuously. Open the session's inbound.db read-only,
            // count rows where status='pending'. If > 0, the agent
            // has work it's about to process (or is processing); fire
            // typing. If 0, the agent is idle waiting for the next
            // user message; stay quiet.
            if !has_pending_inbound(&self.data_root, s.agent_group_id, s.id) {
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

}

/// Cheaply check whether a session has unprocessed inbound rows. The
/// host writes to inbound.db so a read-only handle here is safe. We
/// only need a count; the runner picks up the rows on its next poll
/// and the ticker just needs the boolean.
fn has_pending_inbound(
    data_root: &std::path::Path,
    agent_group_id: ironclaw_types::AgentGroupId,
    session_id: ironclaw_types::SessionId,
) -> bool {
    let paths = SessionPaths::new(data_root, agent_group_id, session_id);
    let Ok(conn) = open_inbound_ro_no_mmap(&paths) else {
        return false;
    };
    messages_in::count_due(&conn).map(|n| n > 0).unwrap_or(false)
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
}
