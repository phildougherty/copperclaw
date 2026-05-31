//! In-process replay harness.
//!
//! The harness is intentionally lean for the v1 acceptance gate: it skips
//! the full `run_host` boot dance (channel mpsc consumers, socket server,
//! signal handling, container manager) and drives the underlying services
//! directly:
//!
//! - A `Router` against an in-memory `CentralDb` seeded from
//!   `central.sql`, writing to per-session inbound DBs under a `tempdir`.
//! - A `DeliveryService` reading the same per-session outbound DBs and
//!   handing rows to a `MockAdapter` that captures every `deliver` call.
//! - For each inbound step, an in-process runner with
//!   `max_turns = Some(1)` driven against a `wiremock`-served Anthropic
//!   endpoint that serves the fixture's pre-recorded SSE event stream.
//!
//! Newer fixtures opt into one of two operational gates by setting
//! `manifest.gates`:
//!
//! - `"approvals"` — installs [`copperclaw_modules::ApprovalsModule`] on
//!   the router's hook chain so unknown senders trigger the pending-
//!   approval prompt instead of reaching the runner. `RouteOutcome::
//!   Pending` no longer aborts the run; the harness records the pending
//!   outcome and skips the per-step runner + delivery.
//! - `"budget"` — instead of running the in-process runner after a
//!   route, the harness drives a [`ContainerManager::tick`] so the
//!   daily-token-cap gate fires (and writes its "budget exhausted"
//!   reply through the session's outbound DB).
//!
//! `manifest.trigger_sweep` runs a single [`SweepService::run_once`]
//! pass before any inbound events are processed. The `scheduled-wake`
//! fixture uses this to deterministically wake a session whose
//! `messages_in` row has a past `process_after`, then runs a turn for
//! that session.
//!
//! The harness exposes three entry points:
//!
//! - `ReplayHarness::new(fixture)` — boot.
//! - `ReplayHarness::run()` — drive each `inbound/NNN-*.json` event
//!   through the pipeline; record the resulting state.
//! - `ReplayHarness::compare()` — diff captured state against the
//!   fixture's `expected/*.jsonl` files using manifest substitutions.
#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use copperclaw_channels_core::{
    testing::MockAdapter, AdapterError, Card, ChannelAdapter, DmHandle,
};
use copperclaw_container_rt::{
    ContainerHandle, ContainerRuntime, ContainerSpec, ImageBuildSpec, RtError,
};
use copperclaw_db::central::CentralDb;
use copperclaw_db::migrate::{run_migrations, MigrationSet};
use copperclaw_db::session::{open_inbound_rw_no_mmap, open_outbound, SessionPaths};
use copperclaw_db::tables::sessions;
use copperclaw_host::container_manager::{
    ContainerManager, ManagerConfig, DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS,
    DEFAULT_STOP_GRACE_SECS,
};
use copperclaw_host_delivery::{
    DeliveryService, FsSessionRoot as DeliveryRoot, SessionRoot as DeliverySessionRoot,
};
use copperclaw_host_router::{
    FsSessionRoot as RouterRoot, RouteOutcome, Router, SessionRoot as RouterSessionRoot,
};
use copperclaw_host_sweep::service::FilesystemSessionRoot as SweepRoot;
use copperclaw_host_sweep::{SessionRoot as SweepSessionRoot, SweepService};
use copperclaw_modules::{ApprovalsModule, Module};
use copperclaw_providers::AnthropicProvider;
use copperclaw_runner::{compaction::CompactionCfg, run_loop, RunnerDeps, RunnerToolCtx};
use copperclaw_types::{
    AgentGroupId, ChannelType, Effort, InboundEvent, OutboundMessage, SessionId, SessionStatus,
};
use rusqlite::Connection;
use std::collections::HashSet;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::diff::{diff_stream, DiffReport, Substitutions};
use crate::fixture::{ClaudeTurn, Fixture, ProviderResponseSpec};

/// Booted harness.
pub struct ReplayHarness {
    pub fixture: Fixture,
    pub tempdir: TempDir,
    pub central: CentralDb,
    pub router: Arc<Router>,
    pub delivery: Arc<DeliveryService>,
    /// Channel-type -> `MockAdapter` map. The harness pre-registers a
    /// `MockAdapter` for the channel named in `manifest.channel` plus a
    /// short builtin allow-list (`cli`, `telegram`, `slack`) so multi-
    /// channel fixtures (e.g. inter-agent fan-out) don't need extra
    /// wiring. `snapshot_delivered` aggregates deliveries across all of
    /// them in registration order.
    pub adapters: Vec<(ChannelType, Arc<MockAdapter>)>,
    pub anthropic_server: MockServer,
    /// Session IDs created or referenced by the router across the run.
    /// Used by `compare` to scan the right per-session DBs for actuals.
    pub touched_sessions: Vec<(AgentGroupId, SessionId)>,
    /// Replay of `inbound/*.json` events as they were driven, so the
    /// `inbound-events.jsonl` diff has a concrete actual stream.
    pub played_inbound: Vec<InboundEvent>,
    /// Per-turn `run_loop` errors that were caught instead of bubbled
    /// (so failure-mode fixtures can still snapshot post-state). The
    /// harness exposes these via `Display` on `DiffReport` only when
    /// `compare()` finds other mismatches; otherwise they're treated as
    /// expected for the fixture.
    pub runner_errors: Vec<String>,
    /// True when the manifest's `gates` includes `"approvals"`. Tracked
    /// so `run()` can install the module exactly once before driving
    /// inbound events.
    use_approvals_gate: bool,
    /// True when the manifest's `gates` includes `"budget"`. Drives the
    /// container manager's spawn classifier instead of the runner.
    use_budget_gate: bool,
    /// Cached container manager for budget-gate fixtures. Reused
    /// across inbound steps so its per-agent-group dedup map survives
    /// (otherwise every step would post a fresh budget-exhausted reply,
    /// defeating the dedup assertion).
    container_manager: tokio::sync::Mutex<Option<Arc<ContainerManager>>>,
}

impl ReplayHarness {
    pub async fn new(fixture: Fixture) -> Result<Self> {
        let tempdir = tempfile::tempdir().context("create harness tempdir")?;
        let central = CentralDb::open_in_memory().context("open in-memory central DB")?;
        {
            let mut conn = central.conn().context("borrow central conn")?;
            run_migrations(&mut conn, MigrationSet::Central)
                .context("run central migrations")?;
            conn.execute_batch(&fixture.central_sql)
                .context("apply fixture central.sql")?;
        }

        let router_root: Arc<dyn RouterSessionRoot + Send + Sync> =
            Arc::new(RouterRoot::new(tempdir.path().to_path_buf()));
        let router = Arc::new(Router::new(central.clone(), router_root));

        let delivery_root: Arc<dyn DeliverySessionRoot> =
            Arc::new(DeliveryRoot::new(tempdir.path().to_path_buf()));

        // Pre-register a deterministic `MockAdapter` for every channel
        // type we expect a fixture to drive. The order is fixed so
        // `snapshot_delivered` aggregates in a stable order across runs.
        let mut channel_types: Vec<ChannelType> = vec![
            ChannelType::new(ChannelType::CLI),
            ChannelType::new("telegram"),
            ChannelType::new("slack"),
        ];
        // Ensure the fixture's own channel is present even if it's some
        // future name (e.g. "discord") not in the built-in list.
        let fixture_ct = ChannelType::new(fixture.manifest.channel.as_str());
        if !channel_types.iter().any(|ct| ct == &fixture_ct) {
            channel_types.push(fixture_ct);
        }

        let mut adapters: Vec<(ChannelType, Arc<MockAdapter>)> = Vec::new();
        let mut initial: Vec<(ChannelType, Arc<dyn ChannelAdapter>)> = Vec::new();
        for ct in channel_types {
            let mock = Arc::new(MockAdapter::new(ct.as_str()));
            // Per-channel max_message_chars cap. The manifest can
            // override; otherwise apply the built-in defaults so the
            // long-message-split fixtures trigger the splitter even
            // without an explicit override. Channels not in the
            // default-cap list report `None` (splitter disabled —
            // matches the trait default).
            let cap = fixture
                .manifest
                .adapter_caps
                .get(ct.as_str())
                .copied()
                .or_else(|| default_cap_for(ct.as_str()));
            let wrapped: Arc<dyn ChannelAdapter> =
                Arc::new(CappedAdapter::new(mock.clone(), cap));
            initial.push((ct.clone(), wrapped));
            adapters.push((ct, mock));
        }
        let delivery =
            DeliveryService::with_default_dispatcher(central.clone(), delivery_root, initial);

        let anthropic_server = MockServer::start().await;
        if fixture.manifest.provider_responses.is_empty() {
            mount_claude_turns(&anthropic_server, &fixture.claude_turns).await;
        } else {
            mount_provider_responses(&anthropic_server, &fixture).await?;
        }

        let use_approvals_gate = fixture
            .manifest
            .gates
            .iter()
            .any(|g| g.eq_ignore_ascii_case("approvals"));
        let use_budget_gate = fixture
            .manifest
            .gates
            .iter()
            .any(|g| g.eq_ignore_ascii_case("budget"));

        Ok(Self {
            fixture,
            tempdir,
            central,
            router,
            delivery,
            adapters,
            anthropic_server,
            touched_sessions: Vec::new(),
            played_inbound: Vec::new(),
            runner_errors: Vec::new(),
            use_approvals_gate,
            use_budget_gate,
            container_manager: tokio::sync::Mutex::new(None),
        })
    }

    /// Drive every `inbound/*.json` event through the pipeline. After
    /// each event the harness:
    ///
    /// 1. Calls `Router::route` and records the touched sessions.
    /// 2. Spawns an in-process runner against the (newly-created)
    ///    per-session DBs with `max_turns = Some(1)` so it processes
    ///    exactly one turn and exits.
    /// 3. Calls `DeliveryService::process_session_once` to drain
    ///    `messages_out` through the `MockAdapter`.
    pub async fn run(&mut self) -> Result<()> {
        if self.use_approvals_gate {
            self.install_approvals_module().await?;
        }

        if !self.fixture.inbound_sql.is_empty() {
            self.apply_inbound_sql()?;
        }

        if self.fixture.manifest.trigger_sweep {
            self.trigger_sweep_pass().await?;
        }

        // Queue any scripted adapter failures (rate-limit, transport,
        // bad-request) BEFORE driving inbound. `MockAdapter` pops these
        // FIFO on each `deliver` call, so a fixture can pin
        // "first delivery returns Rate { retry_after: 7 }, second
        // succeeds" by listing exactly one entry here.
        self.apply_pre_delivery_failures()?;

        let events: Vec<InboundEvent> = self.fixture.inbound.clone();
        for event in events {
            self.played_inbound.push(event.clone());
            let outcome = self
                .router
                .route(event.clone())
                .await
                .context("router.route")?;
            match outcome {
                RouteOutcome::Delivered { sessions } => {
                    for d in sessions {
                        if !self
                            .touched_sessions
                            .iter()
                            .any(|(_, s)| *s == d.session_id)
                        {
                            self.touched_sessions
                                .push((d.agent_group_id, d.session_id));
                        }
                        // Ensure session_routing is populated so delivery
                        // can resolve a destination for runner-emitted
                        // chat rows. The host's container_manager normally
                        // does this; we mirror the minimum here.
                        self.seed_session_routing(d.agent_group_id, d.session_id, &event)?;
                        if !self.use_budget_gate {
                            // Mark running so DeliveryService's active path
                            // would pick up the session if it were running.
                            let _ = sessions::mark_container_running(&self.central, d.session_id);
                        }
                    }
                }
                RouteOutcome::Dropped { reason } => {
                    anyhow::bail!("router dropped event: {reason:?}");
                }
                RouteOutcome::Pending { reason } => {
                    if self.use_approvals_gate {
                        // Expected for the sender-not-approved fixture.
                        // The approvals module's notifier has already
                        // fired through the dispatcher (which is wired
                        // to the same MockAdapter set the harness
                        // captures); skip the per-step runner+delivery
                        // because no session was created.
                        continue;
                    }
                    anyhow::bail!("router deferred event: {reason:?}");
                }
            }

            let (ag, sess) = self
                .touched_sessions
                .last()
                .copied()
                .ok_or_else(|| anyhow!("no session touched by route"))?;

            if self.use_budget_gate {
                // Budget-gate fixtures: drive the container manager's
                // spawn classifier instead of running a turn. The gate
                // refuses to spawn (over cap) and writes the budget-
                // exhausted reply to `messages_out`. Then drain it
                // through the mock adapter.
                self.run_budget_gate(ag).await?;
                self.deliver_session(ag, sess).await?;
            } else {
                // One turn per inbound step. The runner exits when
                // `max_turns` is reached. Failure-mode fixtures may
                // make the runner return Err (e.g. `provider.query()`
                // bailing on a 503 before the in-stream Error path
                // can run). Capture that as state so the diff layer
                // can compare against the fixture's expected post-
                // state instead of panicking.
                if let Err(err) = self.run_one_turn(ag, sess).await {
                    self.runner_errors.push(err.to_string());
                    tracing::warn!(error = %err, "runner errored on turn (captured)");
                }
                // Drain outbound for this session through the mock adapter.
                self.deliver_session(ag, sess).await?;
            }
        }
        // Give any dispatcher-spawned tasks (e.g. the approvals
        // module's notifier dispatch) a tick to reach the mock adapter.
        // The `HostDispatcher` spawns its `adapter.deliver` calls onto
        // the current runtime; without yielding the mock would miss the
        // delivery on tight test runtimes.
        tokio::task::yield_now().await;
        for _ in 0..50 {
            let any_dispatch = self
                .adapters
                .iter()
                .any(|(_, a)| !a.deliveries().is_empty());
            if any_dispatch {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        Ok(())
    }

    async fn install_approvals_module(&self) -> Result<()> {
        // Mirror the host's wiring in `boot::install_modules` for the
        // approvals path: persistent lookup queries `users` and the
        // notifier dispatches a notice through the delivery dispatcher.
        let central_for_lookup = self.central.clone();
        let lookup: copperclaw_modules::approvals::SenderLookup = Arc::new(move |sender| {
            copperclaw_db::tables::users::get_by_identity(
                &central_for_lookup,
                sender.channel_type.as_str(),
                &sender.identity,
            )
            .ok()
            .flatten()
            .is_some()
        });

        let central_for_notifier = self.central.clone();
        let notifier: copperclaw_modules::NewPendingNotifier = Arc::new(
            move |ctx: copperclaw_modules::NewPendingCtx, dispatcher| {
                let Ok(Some(wiring)) =
                    copperclaw_db::tables::messaging_group_agents::list_for_ag(
                        &central_for_notifier,
                        ctx.agent_group_id,
                    )
                    .map(|mut v| v.drain(..).next())
                else {
                    return;
                };
                let Ok(mg) = copperclaw_db::tables::messaging_groups::get(
                    &central_for_notifier,
                    wiring.messaging_group_id,
                ) else {
                    return;
                };
                let text = format!(
                    "Unknown sender pending approval.\nChannel: {}\nIdentity: {}",
                    ctx.sender.channel_type.as_str(),
                    ctx.sender.identity,
                );
                let target = copperclaw_modules::DispatchTarget::channel(
                    mg.channel_type.clone(),
                    mg.platform_id.clone(),
                    None,
                );
                let message = copperclaw_types::OutboundMessage {
                    kind: copperclaw_types::MessageKind::Chat,
                    content: serde_json::json!({"text": text}),
                    files: vec![],
                };
                dispatcher.dispatch(&target, &message);
            },
        );

        let module = ApprovalsModule::new()
            .with_persistent_lookup(lookup)
            .with_new_pending_notifier(notifier);

        // Build a host-context-shaped wiring directly on the router's
        // hook chain so the gate runs in `Router::route`. Capture the
        // dispatcher from the delivery service so the notifier can
        // reach the same `MockAdapter` set the harness scrapes.
        let dispatcher = self.delivery.dispatcher();
        let ctx: Arc<dyn copperclaw_modules::ModuleContext> = Arc::new(
            HarnessModuleContext::new(Arc::clone(&self.router), dispatcher),
        );
        module
            .install(ctx)
            .await
            .map_err(|e| anyhow!("install ApprovalsModule: {e}"))?;
        Ok(())
    }

    /// Apply the fixture's `inbound.sql` (if any) to every active
    /// session's `inbound.db`. The path is opened the same way the
    /// router would: through `SessionPaths` rooted at the harness's
    /// tempdir. `open_inbound` runs the schema migrations on first
    /// touch, so DDL referenced by the SQL (e.g. `messages_in`)
    /// always exists by the time the seed runs.
    fn apply_inbound_sql(&mut self) -> Result<()> {
        use copperclaw_db::session::open_inbound;
        let sessions_active = sessions::list_active(&self.central)
            .context("list_active sessions for inbound.sql seed")?;
        for session in sessions_active {
            let paths = SessionPaths::new(
                self.tempdir.path(),
                session.agent_group_id,
                session.id,
            );
            paths.ensure_dirs().context("ensure session dirs")?;
            let conn = open_inbound(&paths).context("open inbound for seed")?;
            conn.execute_batch(&self.fixture.inbound_sql)
                .context("apply fixture inbound.sql")?;
        }
        Ok(())
    }

    async fn trigger_sweep_pass(&mut self) -> Result<()> {
        let sweep_root: Arc<dyn SweepSessionRoot> =
            Arc::new(SweepRoot::new(self.tempdir.path().to_path_buf()));
        let sweep = SweepService::new(self.central.clone(), sweep_root);
        let report = sweep.run_once().context("sweep.run_once")?;
        // Treat every woken session as touched so the runner + delivery
        // pass picks it up below.
        for sid in report.woken_sessions {
            let session = sessions::get(&self.central, sid)
                .context("load woken session row")?;
            if !self
                .touched_sessions
                .iter()
                .any(|(_, s)| *s == sid)
            {
                self.touched_sessions.push((session.agent_group_id, sid));
            }
            // Run a turn for the woken session and drain delivery.
            // Reproduces the host's "container manager spawns runner
            // → runner processes due message → delivery fans out"
            // sequence without actually spawning a container.
            self.run_one_turn(session.agent_group_id, sid).await?;
            self.deliver_session(session.agent_group_id, sid).await?;
        }
        Ok(())
    }

    async fn run_budget_gate(&self, ag: AgentGroupId) -> Result<()> {
        let mgr = self.budget_manager().await;
        // tick() walks list_active and applies its classifier. The
        // seeded session is `container_status='stopped'` + has pending
        // inbound, so the classifier returns Spawn → maybe_spawn →
        // is_over_budget → posts the budget-exhausted reply through
        // `messages_out`. The mock runtime never sees a spawn call.
        mgr.tick().await.context("container_manager.tick")?;
        let _ = ag;
        Ok(())
    }

    /// Lazily build (and cache) the budget-gate [`ContainerManager`]. The
    /// manager's per-agent-group dedup map is process-local, so the
    /// SAME instance must service every inbound step — otherwise a
    /// dedup assertion (e.g. "second inbound does NOT re-post the
    /// budget-exhausted reply") would always fail.
    async fn budget_manager(&self) -> Arc<ContainerManager> {
        let mut slot = self.container_manager.lock().await;
        if let Some(existing) = slot.as_ref() {
            return Arc::clone(existing);
        }
        let cfg = ManagerConfig {
            install_slug: "replay".into(),
            data_dir: self.tempdir.path().to_path_buf(),
            default_image_tag: "copperclaw/session:replay".into(),
            default_provider: "anthropic".into(),
            default_model: "claude-sonnet-4-6".into(),
            default_effort: None,
            anthropic_api_key: Some("harness".into()),
            anthropic_base_url: Some(self.anthropic_server.uri()),
            idle_timeout_secs: DEFAULT_IDLE_TIMEOUT_SECS,
            heartbeat_stale_secs: DEFAULT_HEARTBEAT_STALE_SECS,
            stop_grace_secs: DEFAULT_STOP_GRACE_SECS,
            skills_dir: None,
            groups_dir: None,
            skills_mode: copperclaw_host::SkillsMode::default(),
            gpu_passthrough: false,
            forward_env: Vec::new(),
        };
        let runtime: Arc<dyn ContainerRuntime> = Arc::new(HarnessRuntime::default());
        let mgr = Arc::new(ContainerManager::new(
            self.central.clone(),
            runtime,
            cfg,
        ));
        *slot = Some(Arc::clone(&mgr));
        mgr
    }

    async fn run_one_turn(&self, ag: AgentGroupId, sess: SessionId) -> Result<()> {
        let paths = SessionPaths::new(self.tempdir.path(), ag, sess);
        paths.ensure_dirs().context("ensure session dirs")?;
        let inbound = open_inbound_rw_no_mmap(&paths).context("open inbound (rw)")?;
        let outbound = open_outbound(&paths).context("open outbound (rw)")?;
        let inbound = Arc::new(Mutex::new(inbound));
        let outbound = Arc::new(Mutex::new(outbound));

        let provider = Arc::new(AnthropicProvider::with_base_url(
            "harness-key",
            self.anthropic_server.uri(),
        ));
        // If the session row records a parent, thread it into the
        // runner ctx the same way production's `main.rs` does — without
        // this, replay fixtures that exercise child sessions would
        // bypass the Phase-2 routing change (Agent-kind default for
        // `to: None`) and silently exercise the legacy path instead.
        let mut tool_ctx_inner = RunnerToolCtx::new(outbound.clone(), paths.outbox.clone());
        if let Ok(s) = copperclaw_db::tables::sessions::get(&self.central, sess) {
            if let Some(parent) = s.source_session_id {
                tool_ctx_inner = tool_ctx_inner.with_source_session_id(parent);
            }
        }
        let tool_ctx: Arc<dyn copperclaw_mcp::ToolContext> = Arc::new(tool_ctx_inner);

        let tool_set = copperclaw_mcp::build_tool_set();
        let tool_defs: Vec<copperclaw_providers::ToolDef> = tool_set
            .iter()
            .map(|e| copperclaw_providers::ToolDef {
                name: e.tool.name.to_string(),
                description: e
                    .tool
                    .description
                    .as_deref()
                    .unwrap_or("")
                    .to_string(),
                input_schema: serde_json::Value::Object((*e.tool.input_schema).clone()),
            })
            .collect();
        let tool_map: Arc<
            std::collections::HashMap<String, Arc<copperclaw_mcp::ToolEntry>>,
        > = Arc::new(
            tool_set
                .into_iter()
                .map(|e| (e.tool.name.to_string(), Arc::new(e)))
                .collect(),
        );

        let deps = RunnerDeps {
            provider,
            tool_ctx,
            inbound,
            outbound,
            tools: tool_defs,
            system: "you are a replay test agent".into(),
            model: "claude-sonnet-4-6".into(),
            effort: Effort::Medium,
            max_tokens: 1024,
            temperature: None,
            assistant_name: Some("Replay".into()),
            compaction: CompactionCfg {
                model_input_window: 200_000,
                safety_margin_tokens: 8_000,
                output_reserve_tokens: 4_096,
                summary_model: "claude-sonnet-4-6".into(),
                summary_effort: Effort::Low,
                summary_max_tokens: 1024,
                archive_dir: paths.outbox.join("_compactions"),
            },
            max_turns: Some(1),
            idle_sleep: Duration::from_millis(10),
            heartbeat_path: Some(paths.heartbeat.clone()),
            session_id: sess,
            agent_group_id: ag,
            turn_seq: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            tool_map,
            max_tool_turns: 5,
            // Short replay-side deadline so the `provider-timeout`
            // fixture (which sets a deliberately long wiremock delay)
            // trips quickly and the retry budget runs to completion
            // inside the per-step `step_timeout_ms` budget.
            provider_deadline: Duration::from_millis(200),
            tool_deadline_secs: 30,
            // Replay tests don't observe the typing-keepalive signal;
            // a noop pinger keeps `RunnerDeps` construction trivial
            // without touching the heartbeat file behind the
            // harness's back.
            activity_pinger: Arc::new(copperclaw_runner::NoopPinger),
            // Replay fixtures don't exercise the slice-3.5 thinking
            // surface — default off.
            surface_thinking: false,
        };
        run_loop(deps).await.context("runner one-turn")?;
        Ok(())
    }

    async fn deliver_session(&self, ag: AgentGroupId, sess: SessionId) -> Result<()> {
        let session = sessions::get(&self.central, sess)
            .context("load session row for delivery")?;
        debug_assert_eq!(session.agent_group_id, ag);
        let _report = self
            .delivery
            .process_session_once(&session)
            .await
            .context("delivery.process_session_once")?;
        // Optional re-drive after a sleep, used by fixtures that pin
        // "row deferred on first tick, delivered on the second tick
        // after waiting `retry_after`". We sleep `redrive_after_ms`
        // (chosen by the fixture to be slightly larger than its
        // queued `Rate { retry_after }`), then run another pass. Any
        // row that's still inside its backoff window will defer again;
        // any row whose window has elapsed will deliver this time.
        if let Some(ms) = self.fixture.manifest.redrive_after_ms {
            tokio::time::sleep(Duration::from_millis(ms)).await;
            let _report = self
                .delivery
                .process_session_once(&session)
                .await
                .context("delivery.process_session_once (redrive)")?;
        }
        Ok(())
    }

    /// Drain `manifest.pre_delivery_failures` into the appropriate
    /// `MockAdapter::fail_next_deliver` queues. Unknown channel names
    /// are reported as a setup error (so a typo doesn't silently
    /// no-op).
    fn apply_pre_delivery_failures(&self) -> Result<()> {
        for entry in &self.fixture.manifest.pre_delivery_failures {
            let mock = self
                .adapters
                .iter()
                .find(|(ct, _)| ct.as_str() == entry.channel)
                .map(|(_, m)| Arc::clone(m))
                .ok_or_else(|| {
                    anyhow!(
                        "pre_delivery_failures references unknown channel \
                         {:?} (registered: {:?})",
                        entry.channel,
                        self.adapters
                            .iter()
                            .map(|(ct, _)| ct.as_str().to_string())
                            .collect::<Vec<_>>(),
                    )
                })?;
            let err = match entry.kind.as_str() {
                "rate" => AdapterError::Rate {
                    retry_after: entry.retry_after,
                },
                "transport" => AdapterError::Transport(
                    entry.message.clone().unwrap_or_else(|| "transport blip".into()),
                ),
                "bad_request" => AdapterError::BadRequest(
                    entry.message.clone().unwrap_or_else(|| "bad request".into()),
                ),
                other => {
                    return Err(anyhow!(
                        "pre_delivery_failures: unknown kind {other:?} \
                         (accepted: rate, transport, bad_request)"
                    ));
                }
            };
            mock.fail_next_deliver(err);
        }
        Ok(())
    }

    fn seed_session_routing(
        &self,
        ag: AgentGroupId,
        sess: SessionId,
        event: &InboundEvent,
    ) -> Result<()> {
        use copperclaw_db::session::open_inbound;
        use copperclaw_db::tables::session_routing;
        use copperclaw_types::routing::SessionRouting;
        let paths = SessionPaths::new(self.tempdir.path(), ag, sess);
        paths.ensure_dirs()?;
        let conn = open_inbound(&paths)?;
        session_routing::write(
            &conn,
            &SessionRouting {
                channel_type: Some(event.channel_type.clone()),
                platform_id: Some(event.platform_id.clone()),
                thread_id: event.thread_id.clone(),
            },
        )?;
        Ok(())
    }

    /// Diff captured actuals against the fixture's expected JSONL streams.
    pub fn compare(&self) -> Result<DiffReport> {
        let subs = Substitutions::compile(&self.fixture.manifest.substitutions)?;
        let mut report = DiffReport::default();

        let actual_inbound: Vec<serde_json::Value> = self
            .played_inbound
            .iter()
            .map(|e| serde_json::to_value(e).expect("inbound to json"))
            .collect();
        report.extend(diff_stream(
            "inbound-events",
            &self.fixture.expected.inbound_events,
            &actual_inbound,
            &subs,
        ));

        let actual_in = self.snapshot_messages_in()?;
        report.extend(diff_stream(
            "messages-in",
            &self.fixture.expected.messages_in,
            &actual_in,
            &subs,
        ));

        let actual_out = self.snapshot_messages_out()?;
        report.extend(diff_stream(
            "messages-out",
            &self.fixture.expected.messages_out,
            &actual_out,
            &subs,
        ));

        let actual_delivered = self.snapshot_delivered();
        report.extend(diff_stream(
            "delivered",
            &self.fixture.expected.delivered,
            &actual_delivered,
            &subs,
        ));
        Ok(report)
    }

    fn snapshot_messages_in(&self) -> Result<Vec<serde_json::Value>> {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut seen: HashSet<SessionId> = HashSet::new();
        for (ag, sess) in &self.touched_sessions {
            if !seen.insert(*sess) {
                continue;
            }
            let paths = SessionPaths::new(self.tempdir.path(), *ag, *sess);
            let conn = open_inbound_rw_no_mmap(&paths)?;
            rows.extend(read_messages_in(&conn)?);
        }
        Ok(rows)
    }

    fn snapshot_messages_out(&self) -> Result<Vec<serde_json::Value>> {
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut seen: HashSet<SessionId> = HashSet::new();
        for (ag, sess) in &self.touched_sessions {
            if !seen.insert(*sess) {
                continue;
            }
            let paths = SessionPaths::new(self.tempdir.path(), *ag, *sess);
            let conn = open_outbound(&paths)?;
            rows.extend(read_messages_out(&conn)?);
        }
        Ok(rows)
    }

    fn snapshot_delivered(&self) -> Vec<serde_json::Value> {
        let mut out: Vec<serde_json::Value> = Vec::new();
        for (ct, adapter) in &self.adapters {
            for d in adapter.deliveries() {
                out.push(serde_json::json!({
                    "channel_type": ct.as_str(),
                    "platform_id": d.platform_id,
                    "thread_id": d.thread_id,
                    "kind": d.message.kind.as_str(),
                    "content": d.message.content,
                    "files": d.message.files.len(),
                }));
            }
        }
        out
    }
}

/// Mount one mock per `provider_responses` entry. Honors three kinds:
///
/// - `success`: serve the SSE turn at `file` (or the i-th legacy
///   `claude/NNN-turn.json` when `file` is omitted).
/// - `error`:   reply with `status` (default 503) and an error body so
///   the upstream `AnthropicProvider` maps it to `ProviderError::Api`.
/// - `timeout`: delay the (still-OK) response by `delay_ms`. Combined
///   with a small per-call deadline on the runner side (when one is
///   added) this simulates an upstream that hangs. Today the runner
///   has no per-call deadline, so timeout fixtures are `#[ignore]`d.
///
/// Each mock has `up_to_n_times(1)` and a unique priority so the i-th
/// upstream request consumes the i-th scripted response in order.
async fn mount_provider_responses(server: &MockServer, fixture: &Fixture) -> Result<()> {
    for (i, spec) in fixture.manifest.provider_responses.iter().enumerate() {
        let pri = u8::try_from(i + 1).unwrap_or(u8::MAX);
        let response = build_provider_response(spec, fixture, i)?;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(response)
            .up_to_n_times(1)
            .with_priority(pri)
            .mount(server)
            .await;
    }
    Ok(())
}

fn build_provider_response(
    spec: &ProviderResponseSpec,
    fixture: &Fixture,
    index: usize,
) -> Result<ResponseTemplate> {
    match spec.kind.as_str() {
        "success" => {
            let turn = lookup_turn(spec, fixture, index)?;
            let body = encode_sse(&turn);
            Ok(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body))
        }
        "error" => {
            let status = spec.status.unwrap_or(503);
            let message = spec
                .message
                .clone()
                .unwrap_or_else(|| "service unavailable".to_string());
            // Anthropic's error envelope shape; the provider's
            // `map_http_error` is robust to anything 5xx so this mainly
            // gives a useful body in test logs.
            let body = serde_json::json!({
                "type": "error",
                "error": { "type": "api_error", "message": message },
            });
            Ok(ResponseTemplate::new(status).set_body_json(body))
        }
        "timeout" => {
            // The wiremock-side delay is best-effort: any caller-side
            // deadline shorter than this will trip first, which is the
            // whole point of the fixture. The dummy turn body keeps
            // wiremock happy if the deadline never fires.
            let delay = Duration::from_millis(spec.delay_ms.unwrap_or(60_000));
            let dummy = ClaudeTurn { events: Vec::new() };
            Ok(ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(encode_sse(&dummy))
                .set_delay(delay))
        }
        other => Err(anyhow!("unknown provider_responses kind: {other}")),
    }
}

fn lookup_turn(
    spec: &ProviderResponseSpec,
    fixture: &Fixture,
    index: usize,
) -> Result<ClaudeTurn> {
    if let Some(name) = spec.file.as_deref() {
        fixture
            .claude_turns_by_name
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow!("no claude turn file named {name} in fixture"))
    } else {
        // Fall back to the i-th turn file in directory order. This
        // matches the legacy behaviour for a fixture that lists only
        // `success` entries.
        fixture
            .claude_turns
            .get(index)
            .cloned()
            .ok_or_else(|| {
                anyhow!(
                    "provider_responses[{index}] has kind=success but no \
                     `file` and only {} turn files were found",
                    fixture.claude_turns.len()
                )
            })
    }
}

/// Mount one mock per claude turn at `/v1/messages`. Each mock has
/// `up_to_n_times(1)` so the i-th request matches the i-th mock.
async fn mount_claude_turns(server: &MockServer, turns: &[ClaudeTurn]) {
    // wiremock priorities are u8 with 1 == highest, 255 == lowest. We
    // want the i-th request to consume the i-th mock; once a mock hits
    // its `up_to_n_times` cap it stops matching, so the next-lowest
    // priority takes over. Clamp the index into [1, 255].
    for (i, turn) in turns.iter().enumerate() {
        let body = encode_sse(turn);
        let pri = u8::try_from(i + 1).unwrap_or(u8::MAX);
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(body),
            )
            .up_to_n_times(1)
            .with_priority(pri)
            .mount(server)
            .await;
    }
}

fn encode_sse(turn: &ClaudeTurn) -> String {
    let mut out = String::new();
    for ev in &turn.events {
        let name = ev
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("message");
        out.push_str("event: ");
        out.push_str(name);
        out.push('\n');
        out.push_str("data: ");
        out.push_str(&serde_json::to_string(ev).expect("serialize SSE event"));
        out.push_str("\n\n");
    }
    out
}

/// Hand-rolled SELECT against `messages_in`. Snapshots every row in
/// ascending `seq` order and renders deterministic JSON the diff can
/// compare against.
fn read_messages_in(conn: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, seq, kind, timestamp, status, process_after, recurrence,
                series_id, tries, trigger, platform_id, channel_type, thread_id,
                content, source_session_id, on_wake
         FROM messages_in
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let content_str: String = row.get("content")?;
        let content: serde_json::Value = serde_json::from_str(&content_str)
            .unwrap_or(serde_json::Value::Null);
        let trigger: i64 = row.get("trigger")?;
        let on_wake: i64 = row.get("on_wake")?;
        Ok(serde_json::json!({
            "id": row.get::<_, String>("id")?,
            "seq": row.get::<_, i64>("seq")?,
            "kind": row.get::<_, String>("kind")?,
            "timestamp": row.get::<_, String>("timestamp")?,
            "status": row.get::<_, String>("status")?,
            "process_after": row.get::<_, Option<String>>("process_after")?,
            "recurrence": row.get::<_, Option<String>>("recurrence")?,
            "series_id": row.get::<_, Option<String>>("series_id")?,
            "tries": row.get::<_, i64>("tries")?,
            "trigger": trigger != 0,
            "platform_id": row.get::<_, Option<String>>("platform_id")?,
            "channel_type": row.get::<_, Option<String>>("channel_type")?,
            "thread_id": row.get::<_, Option<String>>("thread_id")?,
            "content": content,
            "source_session_id": row.get::<_, Option<String>>("source_session_id")?,
            "on_wake": on_wake != 0,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn read_messages_out(conn: &Connection) -> Result<Vec<serde_json::Value>> {
    let mut stmt = conn.prepare(
        "SELECT id, seq, in_reply_to, timestamp, deliver_after, recurrence,
                kind, platform_id, channel_type, thread_id, content
         FROM messages_out
         ORDER BY seq ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        let content_str: String = row.get("content")?;
        let content: serde_json::Value = serde_json::from_str(&content_str)
            .unwrap_or(serde_json::Value::Null);
        Ok(serde_json::json!({
            "id": row.get::<_, String>("id")?,
            "seq": row.get::<_, i64>("seq")?,
            "in_reply_to": row.get::<_, Option<String>>("in_reply_to")?,
            "timestamp": row.get::<_, String>("timestamp")?,
            "deliver_after": row.get::<_, Option<String>>("deliver_after")?,
            "recurrence": row.get::<_, Option<String>>("recurrence")?,
            "kind": row.get::<_, String>("kind")?,
            "platform_id": row.get::<_, Option<String>>("platform_id")?,
            "channel_type": row.get::<_, Option<String>>("channel_type")?,
            "thread_id": row.get::<_, Option<String>>("thread_id")?,
            "content": content,
        }))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

// Suppress unused-symbol clippy warnings when only a subset of features
// of `sessions` are referenced.
#[allow(dead_code)]
fn _ensure_session_status_in_scope(_s: SessionStatus) {}

// ─────────────────────────── support types ───────────────────────────

/// Minimal [`copperclaw_modules::ModuleContext`] used by the harness when
/// it installs `ApprovalsModule` directly on the router. Routes hook
/// registrations to the router's hook chain and exposes the delivery
/// service's dispatcher to `on_delivery_adapter_ready` callbacks.
struct HarnessModuleContext {
    router: Arc<Router>,
    dispatcher: Arc<dyn copperclaw_modules::DeliveryDispatcher>,
}

impl HarnessModuleContext {
    fn new(
        router: Arc<Router>,
        dispatcher: Arc<dyn copperclaw_modules::DeliveryDispatcher>,
    ) -> Self {
        Self { router, dispatcher }
    }
}

#[async_trait]
impl copperclaw_modules::ModuleContext for HarnessModuleContext {
    fn set_sender_resolver(&self, f: copperclaw_modules::context::SenderResolver) {
        self.router.hooks().set_sender_resolver(f);
    }
    fn set_access_gate(&self, f: copperclaw_modules::context::AccessGate) {
        self.router.hooks().set_access_gate(f);
    }
    fn set_sender_scope_gate(&self, f: copperclaw_modules::context::SenderScopeGate) {
        self.router.hooks().set_sender_scope_gate(f);
    }
    fn set_message_interceptor(&self, f: copperclaw_modules::context::MessageInterceptor) {
        self.router.hooks().set_message_interceptor(f);
    }
    fn set_channel_request_gate(&self, f: copperclaw_modules::context::ChannelRequestGate) {
        self.router.hooks().set_channel_request_gate(f);
    }
    fn register_delivery_action(
        &self,
        _name: &str,
        _h: Arc<dyn copperclaw_modules::context::DeliveryActionHandler>,
    ) {
        // The harness's delivery service is constructed via
        // `with_default_dispatcher` with no built-in action handlers.
        // The approvals fixture doesn't exercise the `approval_card`
        // action so it's safe to ignore the registration here.
    }
    fn on_delivery_adapter_ready(&self, cb: copperclaw_modules::context::DeliveryReadyCallback) {
        cb(Arc::clone(&self.dispatcher));
    }
}

/// No-op runtime for the budget-gate fixture. The gate fires before
/// the manager ever asks the runtime to spawn, so the only methods
/// that matter are `remove` (called by `maybe_spawn` defensively) and
/// `stop` (never called on this code path). Every method records nothing
/// and returns success; the harness asserts via `messages_out` rather
/// than runtime telemetry.
#[derive(Debug, Default)]
struct HarnessRuntime {
    spawn_calls: StdMutex<Vec<String>>,
}

impl HarnessRuntime {
    fn spawn_call_count(&self) -> usize {
        self.spawn_calls.lock().unwrap().len()
    }
}

#[async_trait]
impl ContainerRuntime for HarnessRuntime {
    async fn ensure_running(&self) -> Result<(), RtError> {
        Ok(())
    }
    async fn cleanup_orphans(&self, _slug: &str) -> Result<(), RtError> {
        Ok(())
    }
    async fn spawn(&self, spec: ContainerSpec) -> Result<ContainerHandle, RtError> {
        self.spawn_calls.lock().unwrap().push(spec.name.clone());
        Ok(ContainerHandle::new(
            format!("harness-{}-id", spec.name),
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

/// Built-in `max_message_chars` cap per channel type. Mirrors the
/// production `ChannelAdapter::max_message_chars` overrides shipped
/// by the slice-1 cohesive-UX baseline (see `CHANGELOG.md`
/// [Unreleased]) so replay fixtures can pin the host-side splitter
/// behaviour without hand-rolling per-adapter wiring. Returns `None`
/// for channels whose production adapter also returns `None` (cli,
/// matrix, generic webhooks).
fn default_cap_for(channel_type: &str) -> Option<usize> {
    match channel_type {
        "telegram" | "gchat" | "whatsapp-cloud" => Some(4096),
        "slack" => Some(40_000),
        "discord" => Some(2000),
        "teams" => Some(28_000),
        "wechat" => Some(600),
        "webex" => Some(7439),
        "line" => Some(5000),
        _ => None,
    }
}

/// Wrap a [`MockAdapter`] so the harness can report a per-channel
/// `max_message_chars` cap matching production adapter behaviour
/// without modifying the channels-core `MockAdapter` itself.
///
/// Every other trait method delegates straight to the inner mock — so
/// `deliveries()`, `fail_next_deliver()`, edit / reaction capture, and
/// the open-DM hook all continue to work through the harness's
/// `Arc<MockAdapter>` handle held alongside the wrapper.
struct CappedAdapter {
    inner: Arc<MockAdapter>,
    max_message_chars: Option<usize>,
}

impl CappedAdapter {
    fn new(inner: Arc<MockAdapter>, max_message_chars: Option<usize>) -> Self {
        Self {
            inner,
            max_message_chars,
        }
    }
}

#[async_trait]
impl ChannelAdapter for CappedAdapter {
    fn channel_type(&self) -> &ChannelType {
        self.inner.channel_type()
    }

    fn supports_threads(&self) -> bool {
        self.inner.supports_threads()
    }

    fn max_message_chars(&self) -> Option<usize> {
        self.max_message_chars
    }

    async fn subscribe(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.inner.subscribe(platform_id, thread_id).await
    }

    async fn set_typing(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), AdapterError> {
        self.inner.set_typing(platform_id, thread_id).await
    }

    async fn deliver(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        message: &OutboundMessage,
    ) -> Result<Option<String>, AdapterError> {
        self.inner.deliver(platform_id, thread_id, message).await
    }

    async fn deliver_card(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        card: &Card,
        to: Option<&str>,
    ) -> Result<Option<String>, AdapterError> {
        self.inner
            .deliver_card(platform_id, thread_id, card, to)
            .await
    }

    async fn open_dm(&self, user_id: &str) -> Result<Option<DmHandle>, AdapterError> {
        self.inner.open_dm(user_id).await
    }

    async fn edit_message(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        new_text: &str,
    ) -> Result<(), AdapterError> {
        self.inner
            .edit_message(platform_id, thread_id, external_id, new_text)
            .await
    }

    async fn add_reaction(
        &self,
        platform_id: &str,
        thread_id: Option<&str>,
        external_id: &str,
        emoji: &str,
    ) -> Result<(), AdapterError> {
        self.inner
            .add_reaction(platform_id, thread_id, external_id, emoji)
            .await
    }

    fn plain_text_fallback(&self, msg: &OutboundMessage) -> Option<OutboundMessage> {
        self.inner.plain_text_fallback(msg)
    }
}
