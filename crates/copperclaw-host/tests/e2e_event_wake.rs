//! E2E: event-driven wake for idle sessions (M17 C3).
//!
//! A message routed to a session whose container is stopped must spawn a
//! container within ~one poll interval, not a sweep interval. Before C3
//! the container manager only noticed pending inbound on its next poll;
//! now the router signals the manager's wake handle right after the
//! `messages_in` insert and the manager runs an immediate reconcile tick.
//!
//! Like the replay harness (`tests/replay/harness.rs`), this test skips
//! the full `run_host` boot dance and drives the underlying services
//! directly — a real `Router` and a real `ContainerManager::run_loop`
//! sharing one central DB and one session root, with a recording
//! container runtime instead of Docker. The wake wiring under test is
//! exactly what `boot.rs` installs in production
//! (`router.inbound_wake()` -> `ContainerManager::with_wake_notify`).
//!
//! The latency assertion is structured to be deterministic, not merely
//! fast: the manager's poll timer cannot fire before `POLL_INTERVAL_MS`
//! (1000ms) after the loop starts, and the message is routed right after
//! the loop parks — so a spawn observed well inside that window can only
//! have come from the wake path. Polling stays the fallback; this test
//! proves the accelerator.

#![forbid(unsafe_code)]

use async_trait::async_trait;
use copperclaw_container_rt::{
    ContainerHandle, ContainerRuntime, ContainerSpec, ImageBuildSpec, RtError,
};
use copperclaw_db::central::CentralDb;
use copperclaw_host::SkillsMode;
use copperclaw_host::container_manager::{
    ContainerManager, DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS,
    DEFAULT_STOP_GRACE_SECS, ManagerConfig, POLL_INTERVAL_MS,
};
use copperclaw_host_router::{FsSessionRoot, RouteOutcome, Router, SessionRoot};
use copperclaw_types::{ChannelType, ContainerStatus, InboundEvent, InboundMessage, MessageKind};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

/// Container runtime that records the wall-clock instant of every spawn
/// call so the test can assert wake latency. Everything else is a no-op
/// success, mirroring the replay harness's `HarnessRuntime`.
#[derive(Debug, Default)]
struct RecordingRuntime {
    spawns: Mutex<Vec<Instant>>,
}

impl RecordingRuntime {
    fn spawn_instants(&self) -> Vec<Instant> {
        self.spawns.lock().unwrap().clone()
    }
}

#[async_trait]
impl ContainerRuntime for RecordingRuntime {
    async fn ensure_running(&self) -> Result<(), RtError> {
        Ok(())
    }
    async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
        Ok(())
    }
    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
        self.spawns.lock().unwrap().push(Instant::now());
        Ok(ContainerHandle::new(
            format!("wake-{}-id", spec.name),
            spec.name,
        ))
    }
    async fn stop(&self, _name: &str, _grace: Duration) -> Result<(), RtError> {
        Ok(())
    }
    async fn build_image(&self, spec: ImageBuildSpec) -> Result<String, RtError> {
        Ok(spec.image_tag())
    }
}

/// Seed the same shape the replay fixtures use: one agent group, one cli
/// messaging group, one catch-all pattern wiring.
fn seed_central(db: &CentralDb) {
    use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
    use copperclaw_db::tables::messaging_group_agents::{UpsertWiring, upsert as upsert_wire};
    use copperclaw_db::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
    use copperclaw_types::{EngageMode, SessionMode};

    let ag = create_ag(
        db,
        CreateAgentGroup {
            name: "wake-e2e".into(),
            folder: "wake-e2e".into(),
            agent_provider: None,
        },
    )
    .unwrap();
    let mg = upsert_mg(
        db,
        UpsertMessagingGroup {
            channel_type: ChannelType::new("cli"),
            platform_id: "stdin".into(),
            name: None,
            is_group: false,
            unknown_sender_policy: "lenient".into(),
        },
    )
    .unwrap();
    upsert_wire(
        db,
        UpsertWiring {
            messaging_group_id: mg.id,
            agent_group_id: ag.id,
            engage_mode: EngageMode::Pattern,
            engage_pattern: Some(".*".into()),
            sender_scope: "all".into(),
            ignored_message_policy: "drop".into(),
            session_mode: SessionMode::Shared,
            priority: 0,
        },
    )
    .unwrap();
}

fn chat_event(message_id: &str, text: &str) -> InboundEvent {
    InboundEvent {
        channel_type: ChannelType::new("cli"),
        platform_id: "stdin".into(),
        thread_id: None,
        message: InboundMessage {
            id: message_id.into(),
            kind: MessageKind::Chat,
            content: serde_json::json!({"text": text}),
            timestamp: chrono::Utc::now(),
            is_mention: None,
            is_group: None,
        },
        reply_to: None,
        sender: None,
    }
}

#[tokio::test]
async fn inbound_to_stopped_session_spawns_within_a_poll_interval() {
    let tmp = tempfile::tempdir().unwrap();
    let db = CentralDb::open_in_memory().unwrap();
    seed_central(&db);

    let router_root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
    let router = Arc::new(Router::new(db.clone(), router_root));

    let runtime = Arc::new(RecordingRuntime::default());
    let manager_cfg = ManagerConfig {
        install_slug: "wake-e2e".into(),
        data_dir: tmp.path().to_path_buf(),
        default_image_tag: "copperclaw/session:wake-e2e".into(),
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
        skills_mode: SkillsMode::default(),
        gpu_passthrough: false,
        forward_env: Vec::new(),
        egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
    };
    // Production wiring, verbatim: the manager awaits the router's
    // inbound-wake handle alongside its poll timer.
    let manager = Arc::new(
        ContainerManager::new(db.clone(), runtime.clone(), manager_cfg)
            .with_wake_notify(router.inbound_wake()),
    );

    let shutdown = CancellationToken::new();
    let loop_started = Instant::now();
    let manager_task = tokio::spawn(Arc::clone(&manager).run_loop(shutdown.clone()));
    // Let the loop park on its select. Anything the poll timer does now
    // happens no earlier than `loop_started + POLL_INTERVAL_MS`.
    sleep(Duration::from_millis(100)).await;

    // Route one chat message. The router creates the session (container
    // stopped by construction), writes the messages_in row, and signals
    // the wake handle — same as a fresh Telegram/cli inbound in prod.
    let routed_at = Instant::now();
    let out = router.route(chat_event("wake-1", "hello")).await.unwrap();
    let RouteOutcome::Delivered { sessions } = out else {
        panic!("expected delivered, got {out:?}");
    };
    assert_eq!(sessions.len(), 1);
    let target = &sessions[0];
    {
        let row = copperclaw_db::tables::sessions::get(&db, target.session_id).unwrap();
        assert!(
            matches!(row.container_status, ContainerStatus::Stopped),
            "precondition: freshly created session starts with a stopped container"
        );
    }

    // Wait for the spawn. Budget: well inside one poll interval, so a
    // pass can only come from the wake path (the timer arm fires at
    // ~1000ms after loop start at the earliest; we routed ~100ms in).
    let poll_interval = Duration::from_millis(POLL_INTERVAL_MS);
    let budget = poll_interval - Duration::from_millis(300);
    let spawn_at = loop {
        if let Some(first) = runtime.spawn_instants().first().copied() {
            break first;
        }
        assert!(
            routed_at.elapsed() < budget,
            "no spawn within {budget:?} of routing; the wake signal did not accelerate the tick \
             (poll fallback would land at ~{poll_interval:?})"
        );
        sleep(Duration::from_millis(10)).await;
    };
    let latency = spawn_at - routed_at;
    assert!(
        latency < budget,
        "spawn latency {latency:?} must be well inside one poll interval ({poll_interval:?})"
    );
    assert!(
        spawn_at - loop_started < poll_interval,
        "spawn landed before the first poll tick could fire — wake-driven, not poll-driven"
    );

    // No spawn storm: give any stray queued ticks a moment, then assert
    // exactly one spawn and a Running container row.
    sleep(Duration::from_millis(150)).await;
    assert_eq!(
        runtime.spawn_instants().len(),
        1,
        "classify() stays the single decision point — one inbound, one spawn"
    );
    let row = copperclaw_db::tables::sessions::get(&db, target.session_id).unwrap();
    assert!(
        matches!(row.container_status, ContainerStatus::Running),
        "session must be marked running after the wake-driven spawn"
    );

    shutdown.cancel();
    let _ = manager_task.await;
}

/// A second message while the container is already running must not
/// spawn again — the wake permit is consumed by a tick whose classify()
/// sees a healthy Running session and does nothing.
#[tokio::test]
async fn wake_on_running_session_does_not_respawn() {
    let tmp = tempfile::tempdir().unwrap();
    let db = CentralDb::open_in_memory().unwrap();
    seed_central(&db);

    let router_root: Arc<dyn SessionRoot + Send + Sync> = Arc::new(FsSessionRoot::new(tmp.path()));
    let router = Arc::new(Router::new(db.clone(), router_root));
    let runtime = Arc::new(RecordingRuntime::default());
    let manager_cfg = ManagerConfig {
        install_slug: "wake-e2e-2".into(),
        data_dir: tmp.path().to_path_buf(),
        default_image_tag: "copperclaw/session:wake-e2e".into(),
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
        skills_mode: SkillsMode::default(),
        gpu_passthrough: false,
        forward_env: Vec::new(),
        egress_mode: copperclaw_container_rt::EgressMode::AllowAll,
    };
    let manager = Arc::new(
        ContainerManager::new(db.clone(), runtime.clone(), manager_cfg)
            .with_wake_notify(router.inbound_wake()),
    );

    let shutdown = CancellationToken::new();
    let manager_task = tokio::spawn(Arc::clone(&manager).run_loop(shutdown.clone()));
    sleep(Duration::from_millis(50)).await;

    // First message: spawns.
    let out = router.route(chat_event("wake-a", "hello")).await.unwrap();
    assert!(matches!(out, RouteOutcome::Delivered { .. }));
    let deadline = Instant::now() + Duration::from_millis(700);
    while runtime.spawn_instants().is_empty() {
        assert!(Instant::now() < deadline, "first spawn never happened");
        sleep(Duration::from_millis(10)).await;
    }

    // Second message while Running: wakes the loop, but classify() must
    // leave the healthy session alone. A freshly spawned container's
    // heartbeat file doesn't exist yet, but the spawn path stamps
    // last_active, so the crash/idle checks stay quiet within the test
    // window.
    let out = router.route(chat_event("wake-b", "again")).await.unwrap();
    assert!(matches!(out, RouteOutcome::Delivered { .. }));
    sleep(Duration::from_millis(200)).await;
    assert_eq!(
        runtime.spawn_instants().len(),
        1,
        "message to a Running session must not trigger a second spawn"
    );

    shutdown.cancel();
    let _ = manager_task.await;
}
