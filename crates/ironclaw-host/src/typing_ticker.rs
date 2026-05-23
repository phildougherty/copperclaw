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
use ironclaw_db::tables::{messaging_groups, sessions};
use ironclaw_modules::{DeliveryDispatcher, DispatchTarget};
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
    interval: Duration,
}

impl TypingTicker {
    pub fn new(central: CentralDb, dispatcher: Arc<dyn DeliveryDispatcher>) -> Self {
        Self {
            central,
            dispatcher,
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

    fn make_running_session(central: &CentralDb, ch: &str) -> ironclaw_types::SessionId {
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
        s.id
    }

    #[test]
    fn tick_fires_set_typing_for_each_running_session() {
        let (_tmp, central) = fresh_central();
        let _s1 = make_running_session(&central, "telegram");
        let _s2 = make_running_session(&central, "slack");

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
        );
        let fired = ticker.tick();
        assert_eq!(fired, 2, "both running sessions should fire");
        let calls = mock.typing_calls.lock().unwrap();
        let kinds: Vec<&str> = calls
            .iter()
            .map(|t| t.channel_type.as_ref().unwrap().as_str())
            .collect();
        assert!(kinds.contains(&"telegram"));
        assert!(kinds.contains(&"slack"));
    }

    #[test]
    fn tick_skips_idle_sessions() {
        let (_tmp, central) = fresh_central();
        let s = make_running_session(&central, "telegram");
        // Flip to idle: the ticker should NOT fire for this session
        // anymore — typing while idle is a lie.
        sessions::mark_container_idle(&central, s).unwrap();

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            central,
            StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
        );
        let fired = ticker.tick();
        assert_eq!(fired, 0);
    }

    #[test]
    fn tick_skips_sessions_without_messaging_group() {
        let (_tmp, central) = fresh_central();
        // Session with NO messaging_group_id (e.g. an unrouted agent).
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
        );
        assert_eq!(ticker.tick(), 0);
    }

    #[tokio::test]
    async fn run_loop_fires_repeatedly_until_shutdown() {
        let (_tmp, central) = fresh_central();
        let _s = make_running_session(&central, "telegram");

        let mock = StdArc::new(MockDispatcher::default());
        let ticker = Arc::new(
            TypingTicker::new(
                central,
                StdArc::clone(&mock) as Arc<dyn DeliveryDispatcher>,
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
