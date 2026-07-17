//! Cold-start feedback (M21 F1, decision (c)).
//!
//! A first message to a fresh session used to get zero feedback for the
//! entire container spawn — image check/build, boot, runner handshake.
//! The typing ticker only fired for `Running` sessions, so on a slow
//! spawn the user stared at silence and on a failing spawn the first
//! signal was the 300-second sweep apology.
//!
//! Two mechanisms close the gap, both anchored on [`SpawnActivity`] — a
//! small shared registry of spawn attempts currently in flight:
//!
//! 1. **Typing from message one.** [`ContainerManager::maybe_spawn`]
//!    registers each real spawn attempt (after the pending-inbound,
//!    budget, and rate-limit gates, so a deferred spawn never registers)
//!    for exactly as long as the attempt runs. The typing ticker holds
//!    the same registry and widens its gate to sessions mid-spawn with
//!    pending inbound — so the "agent is working" indicator shows within
//!    one tick of the message landing, before the runner is up. A typing
//!    indicator is not a message, so this respects the standing "no
//!    periodic new messages" rejection.
//!
//! 2. **One slow-spawn notice.** Each attempt spawns a watchdog that
//!    fires at [`SLOW_SPAWN_NOTICE_AFTER`] (~first image build/pull
//!    territory). If the attempt is *still* in flight, exactly one
//!    "Setting things up" notice is enqueued through the session's
//!    normal outbound path. The notice is deduped per cold-start
//!    episode: consecutive failing/slow attempts share one notice, and
//!    the flag resets only on a successful spawn — never periodic,
//!    never repeated. A failing spawn still ends in the existing
//!    spawn-attempt-tracker -> sweep-apology path, untouched.
//!
//! State is process-local; a host restart resets it (boot re-baselines
//! container state anyway, matching the S4 crash-loop tracker posture).

use super::ContainerManager;
use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
use copperclaw_types::{Session, SessionId};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// How long a spawn attempt may run before the one slow-spawn notice is
/// posted. 20s is comfortably past a warm spawn (image cached, container
/// boot in low single-digit seconds) and squarely inside first
/// image-build / image-pull territory, where the wait is about to be
/// minutes rather than seconds.
pub const SLOW_SPAWN_NOTICE_AFTER: Duration = Duration::from_secs(20);

/// The one user-visible slow-spawn notice. Posted at most once per
/// cold-start episode (see [`SpawnActivity`]).
pub const SLOW_SPAWN_NOTICE_TEXT: &str =
    "Setting things up — this can take a minute or two on the first message.";

/// Registry of spawn attempts currently in flight, shared between the
/// [`ContainerManager`] (writer) and the typing ticker (reader).
///
/// Each attempt gets a monotonically increasing generation so a stale
/// guard drop or a late watchdog can never clobber / notice a newer
/// attempt. The `noticed` set carries the per-episode slow-notice dedup:
/// an episode is a run of consecutive attempts for a session, ended only
/// by a successful spawn ([`Self::end_episode`]).
pub struct SpawnActivity {
    inner: Mutex<ActivityState>,
}

#[derive(Default)]
struct ActivityState {
    next_generation: u64,
    /// Session -> generation of the attempt currently in flight.
    in_flight: HashMap<SessionId, u64>,
    /// Sessions whose current cold-start episode already got the one
    /// slow-spawn notice. Cleared on successful spawn only.
    noticed: HashSet<SessionId>,
}

impl SpawnActivity {
    /// Fresh, empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(ActivityState::default()),
        }
    }

    /// Register the start of a spawn attempt; returns its generation.
    pub(crate) fn begin(&self, session: SessionId) -> u64 {
        let mut state = self.lock();
        let generation = state.next_generation;
        state.next_generation += 1;
        state.in_flight.insert(session, generation);
        generation
    }

    /// Unregister a finished attempt. A stale generation (a newer
    /// attempt already began) is a no-op.
    pub(crate) fn finish(&self, session: SessionId, generation: u64) {
        let mut state = self.lock();
        if state.in_flight.get(&session) == Some(&generation) {
            state.in_flight.remove(&session);
        }
    }

    /// Watchdog claim for the one slow-spawn notice: `true` iff the
    /// attempt identified by `generation` is still in flight AND this
    /// episode hasn't been noticed yet. Marks the episode noticed
    /// atomically with the check, so concurrent claimers can't both win.
    pub(crate) fn try_claim_notice(&self, session: SessionId, generation: u64) -> bool {
        let mut state = self.lock();
        state.in_flight.get(&session) == Some(&generation) && state.noticed.insert(session)
    }

    /// A successful spawn ends the cold-start episode: the next cold
    /// start may notice again if it is slow too.
    pub(crate) fn end_episode(&self, session: SessionId) {
        self.lock().noticed.remove(&session);
    }

    /// Sessions with a spawn attempt currently in flight — the typing
    /// ticker's mid-spawn gate.
    #[must_use]
    pub fn active_sessions(&self) -> Vec<SessionId> {
        self.lock().in_flight.keys().copied().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ActivityState> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Default for SpawnActivity {
    fn default() -> Self {
        Self::new()
    }
}

/// RAII marker for one spawn attempt. Held across the real spawn work in
/// `maybe_spawn`; dropping it (any return path, including a cancelled
/// future) unregisters the attempt and aborts the slow-spawn watchdog.
pub(crate) struct SpawnAttemptGuard {
    activity: Arc<SpawnActivity>,
    session: SessionId,
    generation: u64,
    watchdog: tokio::task::JoinHandle<()>,
}

impl Drop for SpawnAttemptGuard {
    fn drop(&mut self) {
        self.watchdog.abort();
        self.activity.finish(self.session, self.generation);
    }
}

impl ContainerManager {
    /// Register a real spawn attempt (all pre-spawn gates passed) and
    /// arm its slow-spawn watchdog. The returned guard must be held for
    /// the duration of the attempt.
    pub(super) fn begin_spawn_attempt(&self, session: &Session) -> SpawnAttemptGuard {
        let activity = Arc::clone(&self.spawn_activity);
        let generation = activity.begin(session.id);
        let watchdog_activity = Arc::clone(&activity);
        let data_dir = self.cfg.data_dir.clone();
        let session_id = session.id;
        let agent_group_id = session.agent_group_id;
        let watchdog = tokio::spawn(async move {
            tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER).await;
            // Still in flight past the threshold AND first time this
            // episode -> post the one notice. Any other outcome (attempt
            // finished, episode already noticed) stays silent.
            if watchdog_activity.try_claim_notice(session_id, generation) {
                let paths = SessionPaths::new(&data_dir, agent_group_id, session_id);
                post_slow_spawn_notice(&paths, session_id);
            }
        });
        SpawnAttemptGuard {
            activity,
            session: session.id,
            generation,
            watchdog,
        }
    }
}

/// Enqueue the slow-spawn notice through the session's normal outbound
/// path (same mechanics as the budget/rate-limit cap replies): resolve
/// the reply target from `session_routing`, insert one Chat row into
/// `messages_out`, and let the delivery loop carry it. Best-effort — a
/// missing routing target or a DB error is logged and swallowed; the
/// notice is feedback, never a gate.
///
/// M21 F1 (M1 rider): the `copperclaw_slow_spawn_notices_total` counter is
/// incremented on a successful post below; the spawn-phase duration histogram
/// is the pre-existing `copperclaw_container_spawn_seconds`, observed in
/// `spawn.rs`.
fn post_slow_spawn_notice(paths: &SessionPaths, session: SessionId) {
    let routing = match open_inbound(paths)
        .and_then(|conn| copperclaw_db::tables::session_routing::read(&conn))
    {
        Ok(Some(routing)) => routing,
        Ok(None) => {
            debug!(
                session = %session.as_uuid(),
                "slow-spawn notice skipped: no session_routing target",
            );
            return;
        }
        Err(err) => {
            warn!(
                session = %session.as_uuid(),
                ?err,
                "slow-spawn notice skipped: could not read session routing",
            );
            return;
        }
    };
    let row = copperclaw_db::tables::messages_out::WriteOutbound {
        id: copperclaw_types::MessageId::new(),
        in_reply_to: None,
        timestamp: chrono::Utc::now(),
        deliver_after: None,
        recurrence: None,
        kind: copperclaw_types::MessageKind::Chat,
        platform_id: routing.platform_id.clone(),
        channel_type: routing.channel_type.clone(),
        thread_id: routing.thread_id.clone(),
        content: serde_json::json!({ "text": SLOW_SPAWN_NOTICE_TEXT }),
    };
    match open_outbound(paths)
        .and_then(|conn| copperclaw_db::tables::messages_out::insert(&conn, &row))
    {
        Ok(_) => {
            copperclaw_metrics::inc_slow_spawn_notice();
            info!(
                session = %session.as_uuid(),
                channel_type = ?routing.channel_type,
                "posted slow-spawn notice",
            );
        }
        Err(err) => {
            warn!(
                session = %session.as_uuid(),
                ?err,
                "could not post slow-spawn notice",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::ManagerError;
    use super::super::config::{ManagerConfig, SkillsMode};
    use super::super::spawn::{
        DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
    };
    use super::*;
    use crate::typing_ticker::TypingTicker;
    use async_trait::async_trait;
    use copperclaw_container_rt::{
        ContainerHandle, ContainerRuntime, ContainerSpec, ImageBuildSpec, RtError,
    };
    use copperclaw_db::central::CentralDb;
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messages_in;
    use copperclaw_db::tables::messaging_groups::{self, UpsertMessagingGroup};
    use copperclaw_db::tables::sessions::{self, CreateSession, create as create_session};
    use copperclaw_modules::{DeliveryDispatcher, DispatchTarget, TypingOutcome};
    use copperclaw_types::{ChannelType, ContainerStatus};
    use std::path::PathBuf;
    use std::sync::Mutex as StdMutex;
    use tokio::sync::Notify;
    use tokio::sync::oneshot;

    fn manager_cfg(data_dir: PathBuf) -> ManagerConfig {
        ManagerConfig {
            install_slug: "test".into(),
            data_dir,
            default_image_tag: "copperclaw/session:test".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("sk-test".into()),
            anthropic_base_url: None,
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

    /// Create an agent group + channel-bound messaging group + session
    /// (container `Stopped`), and seed one pending chat inbound plus a
    /// `session_routing` reply target — the exact shape of a first
    /// message to a fresh session waiting on its cold spawn.
    fn cold_session(central: &CentralDb, data_root: &std::path::Path) -> Session {
        let g = create_ag(
            central,
            CreateAgentGroup {
                name: "cold".into(),
                folder: "cold".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg = messaging_groups::upsert(
            central,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("telegram"),
                platform_id: "chat-cold".into(),
                name: Some("cold".into()),
                is_group: false,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        let s = create_session(
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
        let paths = SessionPaths::new(data_root, g.id, s.id);
        paths.ensure_dirs().unwrap();
        let conn = open_inbound(&paths).unwrap();
        messages_in::insert(
            &conn,
            &messages_in::WriteInbound {
                id: copperclaw_types::MessageId::new(),
                kind: copperclaw_types::MessageKind::Chat,
                timestamp: chrono::Utc::now(),
                content: serde_json::json!({ "text": "first message" }),
                trigger: true,
                on_wake: false,
                process_after: None,
                recurrence: None,
                series_id: None,
                platform_id: Some("chat-cold".into()),
                channel_type: Some(ChannelType::new("telegram")),
                thread_id: None,
                source_session_id: None,
                reply_to: None,
                is_group: None,
            },
        )
        .unwrap();
        copperclaw_db::tables::session_routing::write(
            &conn,
            &copperclaw_types::routing::SessionRouting {
                channel_type: Some(ChannelType::new("telegram")),
                platform_id: Some("chat-cold".into()),
                thread_id: None,
            },
        )
        .unwrap();
        s
    }

    /// Outbound rows carrying the slow-spawn notice text.
    fn slow_spawn_notices(data_root: &std::path::Path, s: &Session) -> usize {
        let paths = SessionPaths::new(data_root, s.agent_group_id, s.id);
        let conn = open_outbound(&paths).unwrap();
        copperclaw_db::tables::messages_out::list_due(&conn)
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.content.get("text").and_then(|v| v.as_str()) == Some(SLOW_SPAWN_NOTICE_TEXT)
            })
            .count()
    }

    /// Container runtime whose `spawn` blocks until released — the mock
    /// stand-in for a slow image build / container boot. `entered`
    /// signals the moment the runtime call begins; `release` lets it
    /// finish (successfully, or with an error when `fail_on_release`).
    /// With `hold` false it behaves like the instant no-op runtime.
    #[derive(Default)]
    struct HoldRuntime {
        entered: Notify,
        release: Notify,
        hold: StdMutex<bool>,
        fail_on_release: StdMutex<bool>,
        spawn_count: StdMutex<usize>,
    }

    impl HoldRuntime {
        fn holding() -> Self {
            let rt = Self::default();
            *rt.hold.lock().unwrap() = true;
            rt
        }
        fn set_hold(&self, hold: bool) {
            *self.hold.lock().unwrap() = hold;
        }
        fn set_fail_on_release(&self, fail: bool) {
            *self.fail_on_release.lock().unwrap() = fail;
        }
        fn spawn_count(&self) -> usize {
            *self.spawn_count.lock().unwrap()
        }
    }

    #[async_trait]
    impl ContainerRuntime for HoldRuntime {
        async fn ensure_running(&self) -> Result<(), RtError> {
            Ok(())
        }
        async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
            Ok(())
        }
        async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            *self.spawn_count.lock().unwrap() += 1;
            if *self.hold.lock().unwrap() {
                self.entered.notify_one();
                self.release.notified().await;
                if *self.fail_on_release.lock().unwrap() {
                    return Err(RtError::Container("held spawn failed".into()));
                }
            }
            Ok(ContainerHandle::new(
                format!("hold-{}-id", spec.name),
                spec.name,
            ))
        }
        async fn stop(&self, _name: &str, _grace: std::time::Duration) -> Result<(), RtError> {
            Ok(())
        }
        async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
            Ok(spec.image_tag())
        }
    }

    /// Runtime whose `spawn` always fails immediately — the fast-fail
    /// half of a failing cold start.
    struct FailRuntime;

    #[async_trait]
    impl ContainerRuntime for FailRuntime {
        async fn ensure_running(&self) -> Result<(), RtError> {
            Ok(())
        }
        async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
            Ok(())
        }
        async fn spawn(&self, _spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
            Err(RtError::Container("spawn always fails".into()))
        }
        async fn stop(&self, _name: &str, _grace: std::time::Duration) -> Result<(), RtError> {
            Ok(())
        }
        async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
            Ok(spec.image_tag())
        }
    }

    /// Minimal dispatcher capturing `set_typing` targets.
    #[derive(Default)]
    struct MockDispatcher {
        typing_calls: StdMutex<Vec<DispatchTarget>>,
    }

    impl DeliveryDispatcher for MockDispatcher {
        fn set_typing(&self, target: &DispatchTarget) -> Option<oneshot::Receiver<TypingOutcome>> {
            self.typing_calls.lock().unwrap().push(target.clone());
            None
        }
        fn dispatch(&self, _target: &DispatchTarget, _message: &copperclaw_types::OutboundMessage) {
        }
    }

    fn make_manager(
        db: &CentralDb,
        runtime: Arc<dyn ContainerRuntime>,
        data_dir: &std::path::Path,
        activity: &Arc<SpawnActivity>,
    ) -> Arc<ContainerManager> {
        Arc::new(
            ContainerManager::new(db.clone(), runtime, manager_cfg(data_dir.to_path_buf()))
                .with_spawn_activity(Arc::clone(activity)),
        )
    }

    // ---- SpawnActivity unit behavior -------------------------------------

    #[test]
    fn activity_begin_finish_roundtrip_with_generations() {
        let activity = SpawnActivity::new();
        let s = SessionId::new();
        let gen1 = activity.begin(s);
        assert_eq!(activity.active_sessions(), vec![s]);
        // A newer attempt supersedes; the stale guard's finish is a no-op.
        let gen2 = activity.begin(s);
        activity.finish(s, gen1);
        assert_eq!(
            activity.active_sessions(),
            vec![s],
            "stale-generation finish must not clobber the newer attempt",
        );
        activity.finish(s, gen2);
        assert!(activity.active_sessions().is_empty());
    }

    #[test]
    fn activity_notice_claim_once_per_episode_until_success() {
        let activity = SpawnActivity::new();
        let s = SessionId::new();
        let gen1 = activity.begin(s);
        assert!(activity.try_claim_notice(s, gen1), "first claim wins");
        assert!(
            !activity.try_claim_notice(s, gen1),
            "second claim in the same episode must lose",
        );
        activity.finish(s, gen1);
        // Next (failing-retry) attempt in the same episode: still noticed.
        let gen2 = activity.begin(s);
        assert!(
            !activity.try_claim_notice(s, gen2),
            "episode dedup spans consecutive attempts",
        );
        activity.finish(s, gen2);
        // A successful spawn ends the episode; the next cold start may
        // notice again.
        activity.end_episode(s);
        let gen3 = activity.begin(s);
        assert!(activity.try_claim_notice(s, gen3));
        activity.finish(s, gen3);
    }

    #[test]
    fn activity_claim_requires_in_flight_attempt() {
        let activity = SpawnActivity::new();
        let s = SessionId::new();
        let generation = activity.begin(s);
        activity.finish(s, generation);
        assert!(
            !activity.try_claim_notice(s, generation),
            "a finished attempt (fast spawn) must never claim the notice",
        );
    }

    // ---- Integration: typing during spawn, one notice, apology path ------

    /// Acceptance: an inbound to a stopped session pulses typing while
    /// the container spawn is still in flight — before the runner is up.
    #[tokio::test(start_paused = true)]
    async fn typing_fires_for_mid_spawn_session_before_runner_is_up() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let session = cold_session(&db, tmp.path());
        let runtime = Arc::new(HoldRuntime::holding());
        let activity = Arc::new(SpawnActivity::new());
        let mgr = make_manager(&db, Arc::clone(&runtime) as _, tmp.path(), &activity);

        let task = {
            let mgr = Arc::clone(&mgr);
            let session = session.clone();
            tokio::spawn(async move { mgr.maybe_spawn(&session).await })
        };
        runtime.entered.notified().await;

        // The runner is NOT up: the runtime call is still blocked and the
        // session is still `Stopped`.
        assert_eq!(
            sessions::get(&db, session.id).unwrap().container_status,
            ContainerStatus::Stopped,
        );

        let dispatcher = Arc::new(MockDispatcher::default());
        let ticker = TypingTicker::new(
            db.clone(),
            Arc::clone(&dispatcher) as Arc<dyn DeliveryDispatcher>,
            tmp.path(),
        )
        .with_spawn_activity(Arc::clone(&activity));
        assert_eq!(
            ticker.tick(),
            1,
            "mid-spawn session with pending inbound must pulse typing",
        );
        let calls = dispatcher.typing_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].channel_type.as_ref().unwrap().as_str(), "telegram");

        runtime.release.notify_one();
        assert!(task.await.unwrap().unwrap(), "held spawn completes");
        assert!(
            activity.active_sessions().is_empty(),
            "attempt unregisters when the spawn completes",
        );
        assert_eq!(
            sessions::get(&db, session.id).unwrap().container_status,
            ContainerStatus::Running,
        );
    }

    /// Acceptance: a spawn held past the threshold produces exactly one
    /// notice — not one per tick, not one per extra minute.
    #[tokio::test(start_paused = true)]
    async fn slow_spawn_emits_exactly_one_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let session = cold_session(&db, tmp.path());
        let runtime = Arc::new(HoldRuntime::holding());
        let activity = Arc::new(SpawnActivity::new());
        let mgr = make_manager(&db, Arc::clone(&runtime) as _, tmp.path(), &activity);

        let task = {
            let mgr = Arc::clone(&mgr);
            let session = session.clone();
            tokio::spawn(async move { mgr.maybe_spawn(&session).await })
        };
        runtime.entered.notified().await;
        assert_eq!(slow_spawn_notices(tmp.path(), &session), 0);

        // Cross the threshold (paused clock: auto-advance, zero real wait).
        tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER + Duration::from_secs(1)).await;
        assert_eq!(
            slow_spawn_notices(tmp.path(), &session),
            1,
            "crossing the threshold posts the one notice",
        );

        // Stay slow for minutes more: never a second notice.
        tokio::time::sleep(Duration::from_secs(180)).await;
        assert_eq!(slow_spawn_notices(tmp.path(), &session), 1);

        runtime.release.notify_one();
        assert!(task.await.unwrap().unwrap());
        assert_eq!(slow_spawn_notices(tmp.path(), &session), 1);
    }

    /// Acceptance: a fast spawn produces zero notices.
    #[tokio::test(start_paused = true)]
    async fn fast_spawn_produces_no_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let session = cold_session(&db, tmp.path());
        let runtime = Arc::new(HoldRuntime::default()); // hold=false: instant
        let activity = Arc::new(SpawnActivity::new());
        let mgr = make_manager(&db, Arc::clone(&runtime) as _, tmp.path(), &activity);

        assert!(mgr.maybe_spawn(&session).await.unwrap());
        assert_eq!(runtime.spawn_count(), 1);
        // Give the (aborted) watchdog's window plenty of room to prove
        // nothing fires after the fact.
        tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER * 4).await;
        assert_eq!(
            slow_spawn_notices(tmp.path(), &session),
            0,
            "a fast spawn must never post the slow-spawn notice",
        );
        assert!(activity.active_sessions().is_empty());
    }

    /// Acceptance: a failing spawn still feeds the existing
    /// spawn-attempt-tracker -> sweep-apology path, posts no notice of
    /// its own (fast-fail), and leaves no stale registry entry behind.
    #[tokio::test(start_paused = true)]
    async fn failing_spawn_still_feeds_the_apology_path() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let session = cold_session(&db, tmp.path());
        let activity = Arc::new(SpawnActivity::new());
        let mgr = make_manager(&db, Arc::new(FailRuntime) as _, tmp.path(), &activity);

        let err = mgr.maybe_spawn(&session).await.unwrap_err();
        assert!(matches!(err, ManagerError::Spawn(_)));
        assert_eq!(
            mgr.spawn_tracker().failure_count(session.id),
            1,
            "the failure must land in the tracker the sweep apology reads",
        );
        assert!(
            activity.active_sessions().is_empty(),
            "guard must unregister the attempt on the error path",
        );
        tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER * 2).await;
        assert_eq!(
            slow_spawn_notices(tmp.path(), &session),
            0,
            "a fast-failing attempt never crosses the notice threshold",
        );
        assert_eq!(
            sessions::get(&db, session.id).unwrap().container_status,
            ContainerStatus::Stopped,
        );
    }

    /// Consecutive slow *failing* attempts share one notice (an episode);
    /// only a successful spawn re-arms the notice for a future cold start.
    #[tokio::test(start_paused = true)]
    async fn slow_failing_attempts_notice_once_per_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let db = CentralDb::open_in_memory().unwrap();
        let session = cold_session(&db, tmp.path());
        let runtime = Arc::new(HoldRuntime::holding());
        runtime.set_fail_on_release(true);
        let activity = Arc::new(SpawnActivity::new());
        let mgr = make_manager(&db, Arc::clone(&runtime) as _, tmp.path(), &activity);

        // Attempt 1: slow, then fails -> one notice.
        let task = {
            let mgr = Arc::clone(&mgr);
            let session = session.clone();
            tokio::spawn(async move { mgr.maybe_spawn(&session).await })
        };
        runtime.entered.notified().await;
        tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER + Duration::from_secs(1)).await;
        runtime.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert_eq!(slow_spawn_notices(tmp.path(), &session), 1);

        // Attempt 2 (same episode): slow again, fails again -> still one.
        let task = {
            let mgr = Arc::clone(&mgr);
            let session = session.clone();
            tokio::spawn(async move { mgr.maybe_spawn(&session).await })
        };
        runtime.entered.notified().await;
        tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER + Duration::from_secs(1)).await;
        runtime.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert_eq!(
            slow_spawn_notices(tmp.path(), &session),
            1,
            "retries within one cold-start episode must not repeat the notice",
        );

        // A successful spawn ends the episode...
        runtime.set_fail_on_release(false);
        runtime.set_hold(false);
        assert!(mgr.maybe_spawn(&session).await.unwrap());

        // ...so a future cold start that is slow again gets its own notice.
        sessions::mark_container_stopped(&db, session.id).unwrap();
        runtime.set_hold(true);
        let task = {
            let mgr = Arc::clone(&mgr);
            let session = session.clone();
            tokio::spawn(async move { mgr.maybe_spawn(&session).await })
        };
        runtime.entered.notified().await;
        tokio::time::sleep(SLOW_SPAWN_NOTICE_AFTER + Duration::from_secs(1)).await;
        runtime.release.notify_one();
        assert!(task.await.unwrap().unwrap());
        assert_eq!(
            slow_spawn_notices(tmp.path(), &session),
            2,
            "a new episode after a successful spawn may notice again",
        );
    }
}
