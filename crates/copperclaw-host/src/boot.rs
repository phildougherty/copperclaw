//! Host boot sequence — `copperclaw run` entry point.
//!
//! See `PLAN.md` § 6 T3. The steps in this module mirror the numbered list
//! there.

use crate::channels_init::{DEFAULT_INBOUND_BUFFER, build_registry, init_channels};
use crate::config::HostConfig;
use crate::context::HostContext;
use crate::orphans::cleanup_orphans;
use crate::sessions::FsSessionRoot;
use crate::socket::{bind_listener, serve_listener};
use anyhow::Result;
use copperclaw_channels_core::ChannelAdapter;
use copperclaw_container_rt::{ContainerRuntime, RtError};
use copperclaw_db::central::CentralDb;
use copperclaw_db::migrate::{
    MigrationSet, applied_central_schema_version, expected_central_schema_version, run_migrations,
};
use copperclaw_host_delivery::DeliveryService;
use copperclaw_host_router::Router;
use copperclaw_host_sweep::{SqliteTaskStore, SweepService};
use copperclaw_modules::{
    AgentDispatchModule, AgentToAgentModule, ApprovalsModule, CreateAgentModule, InteractiveModule,
    Module, MountSecurityModule, NewPendingCtx, NewPendingNotifier, PermissionsModule,
    SchedulingModule, SelfModModule, TypingConfig, TypingModule, create_agent_users_table_check,
};
use dashmap::DashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Notification text shown in-channel when an unknown sender is first seen.
///
/// The text is plain ASCII per the project's "no emojis" rule. It is
/// intentionally terse: a longer block would clutter the channel.
const PENDING_SENDER_NOTICE: &str = concat!(
    "Unknown sender pending approval.\n",
    "Run: cclaw approvals approve --channel <channel_type> --identity <identity>",
);

/// Build the closure wired to [`ApprovalsModule::with_new_pending_notifier`].
///
/// The notifier fires synchronously inside the router's sender-scope gate on
/// every inbound from an unknown sender. It must be fast:
///
/// 1. Check [`copperclaw_db::tables::unregistered_senders`] — if a row already
///    exists, the operator was already notified (the router writes the row
///    after the gate returns, so a pre-existing row means the sender has been
///    seen before). Skip.
/// 2. Look up the messaging groups wired to the agent group. Take the first
///    one (ordered by priority desc, then creation time — consistent with the
///    router's own list). If none exist, log at info and return.
/// 3. Dispatch a text notification to that messaging group via the
///    [`copperclaw_modules::DeliveryDispatcher`].
///
/// The dispatcher call itself is synchronous (the host implementation spawns
/// the adapter work in the background), so the gate hot-path is unblocked.
fn build_pending_notifier(central: copperclaw_db::central::CentralDb) -> NewPendingNotifier {
    Arc::new(move |ctx: NewPendingCtx, dispatcher| {
        // De-dupe: if this sender has been seen before, a notification was
        // already posted. The `unregistered_senders` row is written by the
        // router AFTER the gate returns, so absence of the row means this is
        // the sender's first ever contact.
        let already_seen = copperclaw_db::tables::unregistered_senders::get(
            &central,
            &ctx.sender.channel_type,
            &ctx.sender.identity,
        )
        .ok()
        .flatten()
        .is_some();
        if already_seen {
            return;
        }

        // Resolve the primary messaging group for this agent group. "Primary"
        // is defined as the first wiring ordered by priority desc, then
        // created_at asc — the same ordering the router uses in list_for_mg.
        let wirings = match copperclaw_db::tables::messaging_group_agents::list_for_ag(
            &central,
            ctx.agent_group_id,
        ) {
            Ok(w) => w,
            Err(err) => {
                tracing::info!(
                    agent_group_id = %ctx.agent_group_id.as_uuid(),
                    ?err,
                    "approvals: could not list wirings for pending-sender notification; skipping"
                );
                return;
            }
        };
        let Some(wiring) = wirings.first() else {
            tracing::info!(
                agent_group_id = %ctx.agent_group_id.as_uuid(),
                "approvals: agent group has no messaging groups; skipping pending-sender notification"
            );
            return;
        };

        // Resolve the messaging group's channel + platform coordinates.
        let mg = match copperclaw_db::tables::messaging_groups::get(
            &central,
            wiring.messaging_group_id,
        ) {
            Ok(g) => g,
            Err(err) => {
                tracing::info!(
                    messaging_group_id = %wiring.messaging_group_id.as_uuid(),
                    ?err,
                    "approvals: could not fetch messaging group; skipping pending-sender notification"
                );
                return;
            }
        };

        // Build the notification text. Plain ASCII, no emojis.
        let display = ctx
            .sender
            .display_name
            .as_deref()
            .unwrap_or(ctx.sender.identity.as_str());
        let text = format!(
            "{notice}\n\nChannel: {ct}\nIdentity: {id}\nDisplay name: {dn}\nFirst contact: {ts}",
            notice = PENDING_SENDER_NOTICE,
            ct = ctx.sender.channel_type.as_str(),
            id = ctx.sender.identity,
            dn = display,
            ts = ctx.first_seen.to_rfc3339(),
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
        tracing::info!(
            channel_type = ctx.sender.channel_type.as_str(),
            identity = ctx.sender.identity.as_str(),
            notify_channel = mg.channel_type.as_str(),
            notify_platform_id = mg.platform_id.as_str(),
            "approvals: posted pending-sender notification to primary messaging group"
        );
    })
}

/// Boot-time errors that abort startup.
///
/// Maps onto the exit codes documented in the brief:
/// - migrations -> exit 2
/// - runtime detect -> exit 3
/// - schema mismatch (downgrade) -> exit 5
#[derive(Debug, Error)]
pub enum BootError {
    /// Migrations could not be applied.
    #[error("central migrations failed: {0}")]
    Migrate(#[source] copperclaw_db::DbError),
    /// `ContainerRuntime` could not be detected.
    #[error("no container runtime detected: {0}")]
    RuntimeDetect(#[source] RtError),
    /// Opening the central DB failed.
    #[error("open central db failed: {0}")]
    OpenCentral(#[source] copperclaw_db::DbError),
    /// Socket server returned an unexpected I/O error before shutdown.
    #[error("socket server error: {0}")]
    Socket(#[source] std::io::Error),
    /// The on-disk schema is newer than what this binary expects.
    ///
    /// This means a newer copperclaw binary has already migrated the DB and
    /// this (older) binary refuses to touch it to avoid corrupting state.
    /// Upgrade the binary or restore from a backup.
    #[error(
        "schema mismatch: on-disk DB has {applied} applied migrations but \
         this binary only knows {expected}; refusing to run against a future \
         schema (downgrade detected)"
    )]
    SchemaMismatch { expected: usize, applied: usize },
}

impl BootError {
    /// Process exit code to use for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Migrate(_) | Self::OpenCentral(_) => 2,
            Self::RuntimeDetect(_) => 3,
            Self::Socket(_) => 4,
            Self::SchemaMismatch { .. } => 5,
        }
    }
}

/// Run-only-migrations entry point used by `copperclaw migrate`.
pub fn run_migrations_only(cfg: &HostConfig) -> Result<(), BootError> {
    let path = cfg.central_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BootError::OpenCentral(e.into()))?;
    }
    let db = CentralDb::open(&path).map_err(BootError::OpenCentral)?;
    let mut conn = db.conn().map_err(BootError::OpenCentral)?;
    run_migrations(&mut conn, MigrationSet::Central).map_err(BootError::Migrate)?;
    Ok(())
}

/// Check that the on-disk schema version is compatible with this binary.
///
/// - `applied == expected` → log info, continue.
/// - `applied < expected`  → log warn (migrations pending — shouldn't happen
///   after `run_migrations_only`, but defence in depth).
/// - `applied > expected`  → return `Err(BootError::SchemaMismatch)` so the
///   host refuses to boot against a future schema it doesn't understand.
/// - `applied == None` (fresh DB) → treat as 0 applied; if expected > 0 that
///   is "pending", which again shouldn't happen after `run_migrations_only`.
pub fn check_schema_version(cfg: &HostConfig) -> Result<(), BootError> {
    let path = cfg.central_db_path();
    let db = CentralDb::open(&path).map_err(BootError::OpenCentral)?;
    let conn = db.conn().map_err(BootError::OpenCentral)?;
    let expected = expected_central_schema_version();
    let applied = applied_central_schema_version(&conn)
        .map_err(BootError::Migrate)?
        .unwrap_or(0);

    match applied.cmp(&expected) {
        std::cmp::Ordering::Equal => {
            info!(schema_version = applied, "schema version up to date");
        }
        std::cmp::Ordering::Less => {
            warn!(
                applied,
                expected, "schema version behind expected; migrations may be pending"
            );
        }
        std::cmp::Ordering::Greater => {
            return Err(BootError::SchemaMismatch { expected, applied });
        }
    }
    Ok(())
}

/// Migrate old `data_dir/sessions/sessions/<ag>/<session>/` layout to
/// `data_dir/sessions/<ag>/<session>/`.
///
/// The double `sessions/` path was an inadvertent artifact of passing
/// `cfg.sessions_root()` (which returned `data_dir/sessions`) into
/// `FsSessionRoot::new`, while `SessionPaths::new` then appended
/// another `/sessions/<ag>/<session>` on top. This one-shot migrator
/// moves contents of the inner `sessions/` directory up one level and
/// removes the now-empty inner dir.
///
/// Skips (logs + continues) rather than failing when:
/// - The old path doesn't exist (already migrated or fresh install).
/// - A destination path already exists (collision — would overwrite data).
pub fn migrate_sessions_layout(data_dir: &std::path::Path) {
    let old_inner = data_dir.join("sessions").join("sessions");
    if !old_inner.exists() {
        return; // Already on the flat layout or fresh install — nothing to do.
    }
    let new_root = data_dir.join("sessions");
    info!(
        old = %old_inner.display(),
        new = %new_root.display(),
        "migrating double sessions/ path layout"
    );
    let entries = match std::fs::read_dir(&old_inner) {
        Ok(e) => e,
        Err(err) => {
            warn!(?err, "sessions layout migration: read_dir failed; skipping");
            return;
        }
    };
    let mut migrated = 0usize;
    let mut skipped = 0usize;
    for entry in entries.flatten() {
        let src = entry.path();
        let name = entry.file_name();
        let dst = new_root.join(&name);
        if dst.exists() {
            warn!(
                src = %src.display(),
                dst = %dst.display(),
                "sessions layout migration: destination already exists; skipping to avoid collision"
            );
            skipped += 1;
            continue;
        }
        match std::fs::rename(&src, &dst) {
            Ok(()) => migrated += 1,
            Err(err) => {
                warn!(
                    src = %src.display(),
                    dst = %dst.display(),
                    ?err,
                    "sessions layout migration: rename failed; skipping"
                );
                skipped += 1;
            }
        }
    }
    // Only remove the inner dir when we successfully moved everything and there
    // were no skipped entries (skips mean the inner dir may not be empty).
    if skipped == 0 {
        if let Err(err) = std::fs::remove_dir(&old_inner) {
            warn!(
                ?err,
                "sessions layout migration: remove inner dir failed; continuing"
            );
        }
    }
    info!(migrated, skipped, "sessions layout migration complete");
}

/// Construct the assembled host state. Exposed for tests so they can poke
/// individual pieces without spinning up the full event loop.
pub struct HostState {
    pub central: CentralDb,
    pub router: Arc<Router>,
    pub delivery: Arc<DeliveryService>,
    pub sweep: Arc<SweepService>,
    pub session_root: Arc<FsSessionRoot>,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState").finish_non_exhaustive()
    }
}

/// Assemble core services. Stops short of spawning loops — tests use this
/// directly to assert wiring.
pub fn assemble(
    cfg: &HostConfig,
    adapters: DashMap<copperclaw_types::ChannelType, Arc<dyn ChannelAdapter>>,
) -> Result<HostState, BootError> {
    let path = cfg.central_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BootError::OpenCentral(e.into()))?;
    }
    let central = CentralDb::open(&path).map_err(BootError::OpenCentral)?;
    let session_root = Arc::new(FsSessionRoot::new(cfg.sessions_root()));

    let router_root: Arc<dyn copperclaw_host_router::SessionRoot + Send + Sync> =
        Arc::new(FsSessionRoot::new(cfg.sessions_root()));
    let router = Arc::new(Router::new(central.clone(), router_root));

    let delivery_root: Arc<dyn copperclaw_host_delivery::SessionRoot> =
        Arc::new(FsSessionRoot::new(cfg.sessions_root()));
    let dispatcher_map: Arc<DashMap<copperclaw_types::ChannelType, Arc<dyn ChannelAdapter>>> =
        Arc::new(DashMap::new());
    for entry in &adapters {
        dispatcher_map.insert(entry.key().clone(), Arc::clone(entry.value()));
    }
    let resolver_map = Arc::clone(&dispatcher_map);
    let resolver: copperclaw_host_delivery::AdapterResolver = {
        let map = resolver_map;
        Arc::new(move |ct| map.get(ct).map(|r| r.clone()))
    };
    let dispatcher: Arc<dyn copperclaw_modules::DeliveryDispatcher> =
        Arc::new(copperclaw_host_delivery::HostDispatcher::new(resolver));
    let delivery = DeliveryService::new(central.clone(), delivery_root, adapters, dispatcher);

    let sweep_root: Arc<dyn copperclaw_host_sweep::SessionRoot> =
        Arc::new(FsSessionRoot::new(cfg.sessions_root()));
    let sweep = Arc::new(SweepService::new(central.clone(), sweep_root));

    Ok(HostState {
        central,
        router,
        delivery,
        sweep,
        session_root,
    })
}

/// Install the built-in module set against `host_ctx`. Each module that
/// fails to install is logged and skipped.
pub async fn install_modules(host_ctx: Arc<HostContext>, data_root: PathBuf) {
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(TypingModule::new(TypingConfig::default())),
        Box::new(MountSecurityModule::new()),
        Box::new(PermissionsModule::deny_all()),
        // Pre-approve the cli channel's deterministic `local` sender.
        // The cli channel reads the host's own stdin — the only
        // "sender" is the operator running `copperclaw run` already, so
        // there's no meaningful approval gate to apply. Without this
        // pre-seed, every interactive chat would silently deadlock on
        // a missing approval CLI surface.
        //
        // For every other sender, the gate's persistent fallback
        // queries the central `users` table — that's how `cclaw
        // approvals approve` lands without a host restart.
        Box::new(
            ApprovalsModule::with_initial_approved(vec![copperclaw_types::SenderIdentity {
                channel_type: copperclaw_types::ChannelType::new(
                    copperclaw_types::ChannelType::CLI,
                ),
                identity: "local".to_string(),
                display_name: Some("local".to_string()),
            }])
            .with_persistent_lookup({
                let central = host_ctx.central().clone();
                std::sync::Arc::new(move |sender| {
                    let kind = sender.channel_type.as_str();
                    copperclaw_db::tables::users::get_by_identity(&central, kind, &sender.identity)
                        .ok()
                        .flatten()
                        .is_some()
                })
            })
            .with_new_pending_notifier(build_pending_notifier(host_ctx.central().clone())),
        ),
        Box::new(InteractiveModule::default()),
        Box::new(SchedulingModule::with_store(Arc::new(
            SqliteTaskStore::new(host_ctx.central().clone()),
        ))),
        // The legacy unit-struct `AgentToAgentModule` registers nothing
        // (it's an interceptor only). The actual `create_agent` action
        // handler lives in `CreateAgentModule::new`, which we build here
        // with the host's central DB + data root so the spawn lands in
        // the same `agent_groups`/`sessions` tables the container manager
        // already polls. Permission is gated by `users_table_check`: a
        // fresh install with no role grants denies every call (safe
        // default); the operator opens the gate by granting Owner or
        // Admin to an operator user.
        Box::new(AgentToAgentModule),
        Box::new(CreateAgentModule::new(
            host_ctx.central().clone(),
            data_root.clone(),
            create_agent_users_table_check(host_ctx.central().clone()),
        )),
        // Handles `MessageKind::Agent` outbound rows — writes them into
        // the target session's inbound.db. Without this, children's
        // default `send_message` calls (which Phase 2 routes via Agent-
        // kind rows pointing at the parent) get silently dropped by the
        // delivery loop's no-op `agent_dispatch` fallback. See
        // docs/plans/agent-to-agent-routing.md.
        Box::new(AgentDispatchModule::new(
            host_ctx.central().clone(),
            data_root.clone(),
        )),
        Box::new(SelfModModule),
    ];
    for m in modules {
        let name = m.name();
        let ctx: Arc<dyn copperclaw_modules::ModuleContext> =
            Arc::clone(&host_ctx) as Arc<dyn copperclaw_modules::ModuleContext>;
        if let Err(err) = m.install(ctx).await {
            warn!(module = name, ?err, "module install failed; continuing");
        }
    }
}

/// Full host entry point used by `copperclaw run`.
///
/// `runtime` may be provided by the caller (for tests); when `None` the
/// real runtime is detected via [`copperclaw_container_rt::detect`].
///
/// `env_file` is the `.env` path used by the SIGHUP secret-rotation
/// handler. When `Some`, a SIGHUP re-reads this file and updates the
/// container manager's forwarded env vars (provider keys, base URL)
/// so subsequent container spawns pick up rotated values without a
/// host restart. When `None`, SIGHUP is logged but is otherwise a
/// no-op.
#[allow(clippy::too_many_lines)] // Boot sequence is intentionally sequential.
pub async fn run_host(
    cfg: HostConfig,
    runtime: Option<Box<dyn ContainerRuntime>>,
    shutdown: CancellationToken,
    env_file: Option<std::path::PathBuf>,
) -> Result<(), BootError> {
    info!(
        data_dir = %cfg.data_dir.display(),
        socket = %cfg.ncl_socket_path.display(),
        "copperclaw boot starting",
    );

    // 1-4. Migrations.
    run_migrations_only(&cfg)?;

    // 4b. Schema version check. After migration, verify applied == expected
    // (warn on pending, error on downgrade). This is defence-in-depth; the
    // normal case is that run_migrations_only just brought applied == expected.
    check_schema_version(&cfg)?;

    // 4c. Session layout migration. Move data_dir/sessions/sessions/ → data_dir/sessions/
    // if the old double-sessions layout exists. Idempotent and non-fatal per entry.
    migrate_sessions_layout(&cfg.data_dir);

    // 5. Detect container runtime. Wrap in an Arc so we can hand one
    // clone to the orphan-cleanup call and another to the container
    // manager later in this fn.
    let runtime: Arc<dyn ContainerRuntime> = match runtime {
        Some(r) => Arc::from(r),
        None => copperclaw_container_rt::detect()
            .await
            .map_err(BootError::RuntimeDetect)?
            .into(),
    };

    // 6. Orphan cleanup (best effort). Removes any leftover session
    // containers from a previous host process.
    if let Err(err) = cleanup_orphans(runtime.as_ref(), &cfg.install_slug).await {
        warn!(?err, "orphan cleanup failed; continuing boot");
    }

    // 6b. Optional Prometheus metrics endpoint. Reads COPPERCLAW_METRICS_ADDR;
    // no-ops when unset. Warns on bind failure but does not abort boot.
    copperclaw_metrics::maybe_start_server(Some(shutdown.clone())).await;

    // 7. Build channel registry.
    let registry = build_registry();

    // 8. Init channels.
    let (inbound_tx, mut inbound_rx) = mpsc::channel(DEFAULT_INBOUND_BUFFER);
    let initialized = init_channels(&registry, &cfg.channels, inbound_tx, &cfg.data_dir).await;
    let adapters: DashMap<copperclaw_types::ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
    for ch in &initialized {
        adapters.insert(ch.channel_type.clone(), Arc::clone(&ch.adapter));
    }

    // 9. Assemble core services.
    let state = assemble(&cfg, adapters)?;

    // 9b. Reset stale `container_status=running` rows. After the orphan
    // cleanup above, the previous-run's containers no longer exist,
    // but the sessions table may still claim they're alive. Without
    // this reset the container manager skips those sessions forever
    // because it only spawns for `container_status=stopped`.
    if let Ok(running) = copperclaw_db::tables::sessions::list_running(&state.central) {
        for s in &running {
            if let Err(err) =
                copperclaw_db::tables::sessions::mark_container_stopped(&state.central, s.id)
            {
                warn!(session = %s.id.as_uuid(), ?err, "reset to stopped failed");
            }
        }
        if !running.is_empty() {
            info!(
                count = running.len(),
                "reset stale running sessions after orphan cleanup"
            );
        }
    }

    // 9c. Boot-time image health check. Reads the configured default
    // image tag and verifies it (a) exists locally, (b) carries an
    // executable runner binary at the expected path, and (c)
    // optionally that its `copperclaw.fingerprint` label matches the
    // host's runner. Failure does NOT abort the boot — instead, the
    // host enters "degraded" mode: the admin socket stays reachable,
    // each session with pending inbound gets a one-time apology row,
    // and the container manager refuses to spawn new sessions.
    let health_outcome: Option<crate::image_health::HealthDegradedReason> =
        run_boot_image_health_check(&cfg, &state.central).await;

    // 10. Install modules.
    let host_ctx = HostContext::for_router(Arc::clone(&state.router), Arc::clone(&state.delivery));
    install_modules(Arc::clone(&host_ctx), cfg.data_dir.clone()).await;

    // 11. Inbound consumer.
    let router_for_consumer = Arc::clone(&state.router);
    let consumer_shutdown = shutdown.clone();
    let consumer = tokio::spawn(async move {
        loop {
            tokio::select! {
                () = consumer_shutdown.cancelled() => break,
                event = inbound_rx.recv() => {
                    let Some(event) = event else { break; };
                    if let Err(err) = router_for_consumer.route(event).await {
                        warn!(?err, "router::route failed");
                    }
                }
            }
        }
    });

    // 12. Delivery loops.
    let active = tokio::spawn(Arc::clone(&state.delivery).run_active_loop(shutdown.clone()));
    let sweep_delivery = tokio::spawn(Arc::clone(&state.delivery).run_sweep_loop(shutdown.clone()));

    // 13. Sweep loop.
    let sweep_loop = tokio::spawn(Arc::clone(&state.sweep).run_loop(shutdown.clone()));

    // 13b. Typing ticker. Keeps the channel's "agent is working"
    // indicator visible every 4 sec for any session with an active
    // container — fills the gap where `TypingModule` only fires on
    // inbound traffic, so users see a continuous bubble during long
    // tool loops rather than a 5-second flash then silence.
    let typing_ticker = Arc::new(crate::typing_ticker::TypingTicker::new(
        state.central.clone(),
        state.delivery.dispatcher(),
        cfg.data_dir.clone(),
    ));
    let typing_task = tokio::spawn(Arc::clone(&typing_ticker).run_loop(shutdown.clone()));

    // 13c. Todo watcher. Polls each running session's
    // `agent_todos.json` and emits chat notifications when a new plan
    // appears or items complete. Gated by `COPPERCLAW_TODO_NOTIFICATIONS`
    // env var (default off — opt-in for operators who want the
    // step-by-step progress signal in chat).
    let todo_watcher = Arc::new(crate::todo_watcher::TodoWatcher::new(
        state.central.clone(),
        state.delivery.dispatcher(),
        cfg.data_dir.clone(),
    ));
    let todo_task = tokio::spawn(Arc::clone(&todo_watcher).run_loop(shutdown.clone()));

    let spawned = spawn_container_manager(
        &cfg,
        state.central.clone(),
        Arc::clone(&runtime),
        shutdown.clone(),
        Arc::clone(state.sweep.spawn_tracker()),
    );
    let (manager_task, manager_handle): (
        Option<tokio::task::JoinHandle<()>>,
        Option<Arc<crate::container_manager::ContainerManager>>,
    ) = match spawned {
        Some((task, mgr)) => (Some(task), Some(mgr)),
        None => (None, None),
    };

    // 13c. If the boot-time image health check flagged the host as
    // degraded, flip the manager into refuse-spawn mode now so the
    // poll loop never tries to spawn against the stale image.
    if let (Some(reason), Some(mgr)) = (health_outcome.as_ref(), manager_handle.as_ref()) {
        mgr.set_degraded();
        // The metric + apology fan-out already fired inside
        // `enter_degraded_mode` — this is just the manager-side
        // bookkeeping. Re-warn so a quick log-tail surfaces the
        // sticky degraded state right before the ready banner.
        warn!(
            reason = %reason,
            "container manager started in degraded mode; will refuse new spawns"
        );
    }

    // 14. Spawn socket server. Bind synchronously first so a bind
    // failure (stale non-socket file at the path, parent dir unwritable,
    // EADDRINUSE, etc.) surfaces as `BootError::Socket` rather than
    // being swallowed by the spawned task's discarded JoinHandle. The
    // accept loop then runs on the spawned task as before.
    let socket_path = cfg.ncl_socket_path.clone();
    let socket_central = state.central.clone();
    let socket_cancel = shutdown.clone();
    let listener = bind_listener(&socket_path).map_err(BootError::Socket)?;
    let socket_task = tokio::spawn(async move {
        serve_listener(listener, socket_path, socket_central, socket_cancel).await
    });

    print_ready_banner(&cfg, &initialized);
    info!("copperclaw boot complete; idling");

    // 15. Idle until shutdown. SIGHUP triggers a secret-rotation
    // reload on the container manager (when one is spawned) and
    // resumes waiting; only SIGINT/SIGTERM/external-cancel exit.
    wait_for_signal_or_sighup(shutdown.clone(), manager_handle.clone(), env_file).await;

    info!("shutdown requested; cancelling tasks");
    shutdown.cancel();

    // Await all tasks with a 30s deadline.
    let deadline = Duration::from_secs(30);
    let _ = tokio::time::timeout(deadline, async {
        let _ = consumer.await;
        let _ = active.await;
        let _ = sweep_delivery.await;
        let _ = sweep_loop.await;
        let _ = typing_task.await;
        let _ = todo_task.await;
        if let Some(t) = manager_task {
            let _ = t.await;
        }
        let _ = socket_task.await;
    })
    .await;

    Ok(())
}

/// Block until a SIGINT/SIGTERM is observed, or `shutdown` is cancelled
/// externally (whichever comes first). SIGHUP is handled in a loop:
/// each one re-reads `env_file` and applies the change to
/// `manager.reload_env`, then resumes waiting. Only SIGINT, SIGTERM,
/// and external cancellation exit.
///
/// On non-Unix platforms there is no signal support; the function
/// blocks on `shutdown.cancelled()` only.
pub async fn wait_for_signal_or_sighup(
    shutdown: CancellationToken,
    manager: Option<Arc<crate::container_manager::ContainerManager>>,
    env_file: Option<std::path::PathBuf>,
) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "could not install SIGINT handler");
                shutdown.cancelled().await;
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(err) => {
                warn!(?err, "could not install SIGTERM handler");
                shutdown.cancelled().await;
                return;
            }
        };
        let mut sighup = match signal(SignalKind::hangup()) {
            Ok(s) => Some(s),
            Err(err) => {
                warn!(
                    ?err,
                    "could not install SIGHUP handler; secret rotation on SIGHUP unavailable"
                );
                None
            }
        };
        loop {
            if let Some(ref mut hup) = sighup {
                tokio::select! {
                    _ = sigint.recv() => { info!("SIGINT received"); return; }
                    _ = sigterm.recv() => { info!("SIGTERM received"); return; }
                    () = shutdown.cancelled() => { info!("external cancellation"); return; }
                    _ = hup.recv() => {
                        info!("SIGHUP received; reloading .env for secret rotation");
                        if let Some(ref mgr) = manager {
                            let changed = mgr.reload_env(env_file.as_deref());
                            if changed.is_empty() {
                                info!("SIGHUP: no secret vars changed");
                            } else {
                                info!(keys = ?changed, "SIGHUP: secret vars rotated (key names only)");
                            }
                        } else {
                            info!("SIGHUP: container manager not running; env reload skipped");
                        }
                    }
                }
            } else {
                tokio::select! {
                    _ = sigint.recv() => { info!("SIGINT received"); return; }
                    _ = sigterm.recv() => { info!("SIGTERM received"); return; }
                    () = shutdown.cancelled() => { info!("external cancellation"); return; }
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = manager;
        let _ = env_file;
        let _ = shutdown.cancelled().await;
    }
}

/// Backwards-compat wrapper: SIGINT/SIGTERM only, no SIGHUP handling.
/// Used by tests that don't need rotation. Production code should
/// prefer [`wait_for_signal_or_sighup`].
pub async fn wait_for_signal(shutdown: CancellationToken) {
    wait_for_signal_or_sighup(shutdown, None, None).await;
}

/// Path under which channels keep their per-instance data directory. Exposed
/// here so other crates can resolve the same location.
pub fn channel_data_dir(data_root: &std::path::Path, channel_type: &str) -> PathBuf {
    data_root.join("channels").join(channel_type)
}

/// Result of [`spawn_container_manager`]: the task handle (driving
/// the poll loop) and the `Arc` the SIGHUP handler uses to call
/// `reload_env` on rotation.
type SpawnedManager = (
    tokio::task::JoinHandle<()>,
    Arc<crate::container_manager::ContainerManager>,
);

/// Spawn the container-manager task. Returns `Some((handle, manager))`
/// when the default image tag is known; otherwise logs a warning and
/// returns `None` so the host still boots (sessions just won't get
/// a runner). The returned `manager` is what the SIGHUP handler holds
/// to apply secret rotation.
fn spawn_container_manager(
    cfg: &HostConfig,
    central: copperclaw_db::central::CentralDb,
    runtime: Arc<dyn ContainerRuntime>,
    shutdown: CancellationToken,
    spawn_tracker: Arc<copperclaw_host_sweep::SpawnAttemptTracker>,
) -> Option<SpawnedManager> {
    let Some(image_tag) = cfg.default_image_tag.clone() else {
        warn!(
            "no COPPERCLAW_DEFAULT_IMAGE_TAG configured; container manager disabled. \
             Sessions will accept inbound but no agent will respond. Run \
             `copperclaw-setup` to build the image and write the tag to .env."
        );
        return None;
    };
    let manager_cfg = crate::container_manager::ManagerConfig {
        install_slug: cfg.install_slug.clone(),
        // sessions_root() returns data_dir itself; SessionPaths::new
        // appends sessions/<ag>/<session> to produce data_dir/sessions/<ag>/<session>.
        // The router/delivery/sweep use FsSessionRoot::new(cfg.sessions_root())
        // which resolves to the same path.
        data_dir: cfg.sessions_root(),
        default_image_tag: image_tag,
        default_provider: cfg
            .default_provider
            .clone()
            .unwrap_or_else(|| "anthropic".into()),
        default_model: cfg
            .default_model
            .clone()
            .unwrap_or_else(|| "claude-sonnet-4-6".into()),
        default_effort: parse_effort_env(),
        anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
        idle_timeout_secs: crate::container_manager::DEFAULT_IDLE_TIMEOUT_SECS,
        heartbeat_stale_secs: crate::container_manager::DEFAULT_HEARTBEAT_STALE_SECS,
        stop_grace_secs: crate::container_manager::DEFAULT_STOP_GRACE_SECS,
        skills_dir: cfg.skills_dir.clone(),
        groups_dir: cfg.groups_dir.clone(),
        skills_mode: cfg.skills_mode,
        gpu_passthrough: parse_truthy_env("COPPERCLAW_CONTAINER_GPU"),
        forward_env: collect_forward_env(),
    };

    // Startup safety check: the host's heartbeat-staleness threshold
    // must leave the runner enough room to fail a provider call
    // cleanly before being declared dead. If an operator pinned
    // COPPERCLAW_RUNNER_PROVIDER_DEADLINE_MS at a value that's too close
    // to (or larger than) `heartbeat_stale_secs / 2`, the host can
    // race the runner and SIGKILL the container the same instant the
    // provider call returns `DeadlineExceeded`. We warn rather than
    // panic — an operator may have set both deliberately (e.g. a
    // local Ollama setup with both numbers cranked) — but the log
    // line names both values so it's actionable. See
    // `container_manager::spawn::check_heartbeat_deadline_alignment`
    // for the rationale. Reads the same env var the runner reads at
    // spawn so the comparison reflects the value the runner will
    // actually be configured with.
    {
        let env_for_check = copperclaw_runner::config::SystemEnv;
        let provider_deadline = copperclaw_runner::resolve_provider_deadline(&env_for_check);
        let provider_deadline_ms = u64::try_from(provider_deadline.as_millis()).unwrap_or(u64::MAX);
        if let Err(msg) = crate::container_manager::spawn::check_heartbeat_deadline_alignment(
            manager_cfg.heartbeat_stale_secs,
            provider_deadline_ms,
        ) {
            warn!(
                heartbeat_stale_secs = manager_cfg.heartbeat_stale_secs,
                provider_deadline_ms, "{msg}"
            );
        }
    }
    let manager = Arc::new(
        crate::container_manager::ContainerManager::new(central, runtime, manager_cfg)
            .with_spawn_tracker(spawn_tracker),
    );
    let task = tokio::spawn(Arc::clone(&manager).run_loop(shutdown));
    Some((task, manager))
}

/// Run the boot-time image health check against
/// `cfg.default_image_tag`. Returns `Some(reason)` when the host
/// should enter degraded mode (and writes the apology rows + sets
/// the metric gauge as a side effect); returns `None` on success
/// (image is healthy or check was skipped because no tag is
/// configured).
///
/// Lives outside [`run_host`] so the call surface is unit-testable
/// (see `crates/copperclaw-host/src/image_health.rs::tests`).
async fn run_boot_image_health_check(
    cfg: &HostConfig,
    central: &copperclaw_db::central::CentralDb,
) -> Option<crate::image_health::HealthDegradedReason> {
    let Some(image_tag) = cfg.default_image_tag.as_deref() else {
        // No configured tag → container manager is already disabled,
        // separate warning is emitted from spawn_container_manager.
        // No need to run the health check in that case.
        return None;
    };
    let probe = crate::image_health::DockerImageProbe;
    let host_fp = crate::image_health::host_runner_fingerprint(
        crate::image_health::default_host_runner_path().as_deref(),
    );
    match crate::image_health::check_image_health(&probe, image_tag, host_fp.as_deref()).await {
        Ok(()) => {
            info!(image_tag = %image_tag, "boot image health check passed");
            None
        }
        Err(reason) => {
            // Side-effects of degraded mode (metric + apology
            // fan-out) live in `enter_degraded_mode`. Calling it from
            // here keeps boot.rs lean.
            let notified =
                crate::image_health::enter_degraded_mode(central, cfg.data_dir(), &reason);
            warn!(
                image_tag = %image_tag,
                reason = %reason,
                notified,
                "boot image health check failed; entering degraded mode"
            );
            Some(reason)
        }
    }
}

/// Collect operator-supplied env vars that should be forwarded into
/// every spawned session container. Today this is the web-search
/// provider keys + the explicit provider override; the list is kept
/// here (rather than spread across the modules that need them) so
/// the host has one place to audit what leaks into the container.
/// Parse a boolean-ish env var. Truthy: `1`, `true`, `yes`, `on`, `all`
/// (case-insensitive). Anything else (incl. unset, empty) is false.
fn parse_truthy_env(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some(
            "1" | "true"
                | "True"
                | "TRUE"
                | "yes"
                | "Yes"
                | "YES"
                | "on"
                | "On"
                | "ON"
                | "all"
                | "All"
                | "ALL"
        )
    )
}

/// Parse `COPPERCLAW_DEFAULT_EFFORT` env var into an `Effort` tier.
/// Recognised values (case-insensitive): `low`, `medium`, `high`.
/// `None` (unset, empty, or unrecognised) means "use the model's
/// default" — no `reasoning.effort` field is emitted on the wire.
/// Unrecognised values log a one-time warning at boot.
fn parse_effort_env() -> Option<copperclaw_types::Effort> {
    let raw = std::env::var("COPPERCLAW_DEFAULT_EFFORT").ok()?;
    match raw.to_ascii_lowercase().as_str() {
        "" => None,
        "low" => Some(copperclaw_types::Effort::Low),
        "medium" | "med" => Some(copperclaw_types::Effort::Medium),
        "high" => Some(copperclaw_types::Effort::High),
        other => {
            tracing::warn!(
                value = %other,
                "COPPERCLAW_DEFAULT_EFFORT must be one of low|medium|high; ignoring"
            );
            None
        }
    }
}

fn collect_forward_env() -> Vec<(String, String)> {
    const FORWARDED: &[&str] = &[
        // Web-search providers (web_search tool).
        "COPPERCLAW_WEB_SEARCH_PROVIDER",
        "TAVILY_API_KEY",
        "EXA_API_KEY",
        "BRAVE_SEARCH_API_KEY",
        "SERPAPI_API_KEY",
        // Codex subprocess provider configuration. The runner's
        // codex arm sources its binary path + args from these (with
        // hard-coded `/usr/local/bin/codex` + `["--json"]` as the
        // ultimate fallback).
        "COPPERCLAW_CODEX_BINARY",
        "COPPERCLAW_CODEX_ARGS",
        // Ollama native provider base URL. Without this, the runner
        // inside the container falls back to `http://localhost:11434`
        // — which inside Docker resolves to the container itself,
        // never the host's Ollama. Forwarding it lets the operator
        // set `OLLAMA_BASE_URL=http://172.17.0.1:11434` (or
        // `host.docker.internal`) in the install's .env.
        "OLLAMA_BASE_URL",
        // UX visibility flags read by the runner inside the container.
        // The runner's `RunnerToolCtx::with_breadcrumbs_from_env()`
        // checks `COPPERCLAW_TOOL_BREADCRUMBS`; without forwarding the
        // operator's `.env` value never reaches the container and the
        // flag silently no-ops.
        "COPPERCLAW_TOOL_BREADCRUMBS",
        // Breadcrumb presentation style read by the runner: `rolling`
        // collapses a turn's tools into one expandable activity chip
        // (else legacy per-tool chips). Same forward-or-no-op caveat.
        "COPPERCLAW_BREADCRUMB_STYLE",
        // Per-session turn cap override. The runner main reads this
        // to size `max_tool_turns` (default 60); operators bump it for
        // long build/research sessions that would otherwise bail mid-flight.
        "COPPERCLAW_MAX_TOOL_TURNS",
    ];
    let mut out = Vec::with_capacity(FORWARDED.len());
    for key in FORWARDED {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                out.push(((*key).to_string(), v));
            }
        }
    }
    out
}

/// Print a one-screen summary of the running host so an operator can see
/// what's wired without scrolling through tracing output.
///
/// The banner is written to stderr alongside the tracing logs so that
/// stdout stays clean for the cli channel.
pub(crate) fn print_ready_banner(
    cfg: &HostConfig,
    channels: &[crate::channels_init::InitializedChannel],
) {
    let lines = ready_banner_lines(cfg, channels);
    let mut stderr = std::io::stderr().lock();
    for line in lines {
        let _ = std::io::Write::write_all(&mut stderr, line.as_bytes());
        let _ = std::io::Write::write_all(&mut stderr, b"\n");
    }
}

/// Pure formatter for [`print_ready_banner`]. Lives separately so it can be
/// unit-tested without poking real stderr.
#[must_use]
pub fn ready_banner_lines(
    cfg: &HostConfig,
    channels: &[crate::channels_init::InitializedChannel],
) -> Vec<String> {
    let channels = if channels.is_empty() {
        "(none)".to_string()
    } else {
        channels
            .iter()
            .map(|c| c.channel_type.as_str().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };
    vec![
        format!("copperclaw {} ready", env!("CARGO_PKG_VERSION")),
        format!("  data:     {}", cfg.data_dir.display()),
        format!("  socket:   {}", cfg.ncl_socket_path.display()),
        format!("  channels: {channels}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boot_error_exit_codes() {
        assert_eq!(
            BootError::Migrate(copperclaw_db::DbError::NotFound).exit_code(),
            2
        );
        assert_eq!(
            BootError::OpenCentral(copperclaw_db::DbError::NotFound).exit_code(),
            2
        );
        assert_eq!(
            BootError::RuntimeDetect(RtError::Unavailable("x".into())).exit_code(),
            3
        );
        assert_eq!(BootError::Socket(std::io::Error::other("x")).exit_code(), 4);
        assert_eq!(
            BootError::SchemaMismatch {
                expected: 4,
                applied: 7
            }
            .exit_code(),
            5
        );
    }

    #[test]
    fn boot_error_display_renders() {
        assert!(
            BootError::Migrate(copperclaw_db::DbError::NotFound)
                .to_string()
                .contains("migrations failed")
        );
        assert!(
            BootError::RuntimeDetect(RtError::Unavailable("x".into()))
                .to_string()
                .contains("no container runtime")
        );
        let msg = BootError::SchemaMismatch {
            expected: 4,
            applied: 7,
        }
        .to_string();
        assert!(msg.contains("downgrade"), "expected 'downgrade' in: {msg}");
        assert!(msg.contains('4') && msg.contains('7'));
    }

    // --- schema version check ------------------------------------------------

    #[test]
    fn check_schema_version_ok_after_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ..HostConfig::default()
        };
        run_migrations_only(&cfg).unwrap();
        check_schema_version(&cfg).unwrap(); // must not error
    }

    #[test]
    fn check_schema_version_errors_on_future_schema() {
        use copperclaw_db::central::CentralDb;

        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ..HostConfig::default()
        };
        run_migrations_only(&cfg).unwrap();
        // Inject a future migration row so applied > expected.
        {
            let db = CentralDb::open(cfg.central_db_path()).unwrap();
            let conn = db.conn().unwrap();
            conn.execute(
                "INSERT INTO schema_version (name, applied) \
                 VALUES ('999_future', '2099-01-01T00:00:00Z')",
                [],
            )
            .unwrap();
        }
        let err = check_schema_version(&cfg).unwrap_err();
        assert!(matches!(err, BootError::SchemaMismatch { .. }));
        assert_eq!(err.exit_code(), 5);
    }

    // --- session layout migration --------------------------------------------

    #[test]
    fn migrate_sessions_layout_noop_on_flat_layout() {
        let tmp = tempfile::tempdir().unwrap();
        // No inner sessions/sessions/ dir — should be a no-op.
        migrate_sessions_layout(tmp.path());
        // No new files created.
        assert!(!tmp.path().join("sessions").exists());
    }

    #[test]
    fn migrate_sessions_layout_moves_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let old_inner = tmp.path().join("sessions").join("sessions");
        let agent_dir = old_inner.join("agent-1234");
        std::fs::create_dir_all(&agent_dir).unwrap();
        std::fs::write(agent_dir.join("inbound.db"), b"fake").unwrap();

        migrate_sessions_layout(tmp.path());

        // Content moved to the flat location.
        let new_agent = tmp.path().join("sessions").join("agent-1234");
        assert!(new_agent.exists(), "agent dir should exist at flat path");
        assert!(new_agent.join("inbound.db").exists());
        // Inner sessions/ dir removed.
        assert!(!old_inner.exists(), "inner sessions/ dir should be gone");
    }

    #[test]
    fn migrate_sessions_layout_skips_on_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let old_inner = tmp.path().join("sessions").join("sessions");
        let old_agent = old_inner.join("agent-abc");
        std::fs::create_dir_all(&old_agent).unwrap();
        // Pre-create the destination so it collides.
        let new_agent = tmp.path().join("sessions").join("agent-abc");
        std::fs::create_dir_all(&new_agent).unwrap();
        std::fs::write(new_agent.join("existing.db"), b"keep").unwrap();

        migrate_sessions_layout(tmp.path());

        // Collision was logged + skipped — existing data intact.
        assert!(new_agent.join("existing.db").exists());
        // The old inner dir still exists because we skipped entries.
        // (Whether it stays or goes depends on the skipped count > 0 guard.)
        assert!(
            old_inner.exists(),
            "inner dir should remain when there were skips"
        );
    }

    #[test]
    fn migrate_sessions_layout_idempotent_after_successful_migration() {
        let tmp = tempfile::tempdir().unwrap();
        let old_inner = tmp.path().join("sessions").join("sessions");
        let agent_dir = old_inner.join("agent-xyz");
        std::fs::create_dir_all(&agent_dir).unwrap();

        migrate_sessions_layout(tmp.path()); // first call moves + removes inner
        // Second call: old_inner no longer exists → no-op.
        migrate_sessions_layout(tmp.path()); // must not panic
    }

    #[test]
    fn ready_banner_includes_paths_and_no_channels_marker() {
        let cfg = HostConfig {
            data_dir: PathBuf::from("/srv/iron/data"),
            ncl_socket_path: PathBuf::from("/srv/iron/data/cclaw.sock"),
            ..HostConfig::default()
        };
        let lines = ready_banner_lines(&cfg, &[]);
        assert!(lines[0].contains("copperclaw"));
        assert!(lines.iter().any(|l| l.contains("/srv/iron/data")));
        assert!(lines.iter().any(|l| l.contains("cclaw.sock")));
        assert!(lines.iter().any(|l| l.contains("(none)")));
    }

    #[test]
    fn ready_banner_lists_channels() {
        use crate::channels_init::InitializedChannel;
        use copperclaw_channels_core::testing::MockAdapter;
        use copperclaw_types::ChannelType;
        let cfg = HostConfig::default();
        let mk = |name: &str| InitializedChannel {
            channel_type: ChannelType::from(name),
            adapter: Arc::new(MockAdapter::new(name)),
        };
        let lines = ready_banner_lines(&cfg, &[mk("cli"), mk("telegram")]);
        let joined = lines.join("\n");
        assert!(joined.contains("cli, telegram"), "actual: {joined}");
    }

    #[test]
    fn run_migrations_only_creates_db_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ..HostConfig::default()
        };
        run_migrations_only(&cfg).unwrap();
        assert!(cfg.central_db_path().exists());
    }

    #[tokio::test]
    async fn assemble_builds_state() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ..HostConfig::default()
        };
        let state = assemble(&cfg, DashMap::new()).unwrap();
        assert!(state.session_root.data_root().starts_with(tmp.path()));
        let _ = state.central.conn().unwrap();
    }

    #[tokio::test]
    async fn host_state_debug() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ..HostConfig::default()
        };
        let state = assemble(&cfg, DashMap::new()).unwrap();
        let s = format!("{state:?}");
        assert!(s.contains("HostState"));
    }

    #[test]
    fn channel_data_dir_helper() {
        let p = channel_data_dir(std::path::Path::new("data"), "cli");
        assert_eq!(p, PathBuf::from("data/channels/cli"));
    }

    #[tokio::test]
    async fn install_modules_via_router_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ..HostConfig::default()
        };
        let state = assemble(&cfg, DashMap::new()).unwrap();
        let ctx = HostContext::for_router(Arc::clone(&state.router), Arc::clone(&state.delivery));
        install_modules(ctx, cfg.data_dir.clone()).await;
        // At least permissions+approvals install hooks; assert something
        // landed on the router's chain.
        assert!(
            state.router.hooks().has_access_gate() || state.router.hooks().has_sender_scope_gate()
        );
    }

    #[tokio::test]
    async fn wait_for_signal_returns_on_external_cancel() {
        let token = CancellationToken::new();
        let cloned = token.clone();
        let t = tokio::spawn(async move { wait_for_signal(cloned).await });
        token.cancel();
        tokio::time::timeout(Duration::from_secs(2), t)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn run_host_boots_with_noop_runtime_and_idles() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            channels: Vec::new(), // no channels -> no per-channel scaffold
            ..HostConfig::default()
        };
        let shutdown = CancellationToken::new();
        let rt: Box<dyn ContainerRuntime> = Box::new(crate::tests::NoopRuntime::default());
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_host(cfg, Some(rt), cancel, None).await.unwrap();
        });
        // Wait briefly so the socket file appears.
        for _ in 0..80 {
            if tmp.path().join("cclaw.sock").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            tmp.path().join("cclaw.sock").exists(),
            "socket should be up"
        );
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn run_host_returns_socket_error_on_bad_socket_path() {
        // The socket sits inside a regular file masquerading as the
        // parent directory — `bind_listener` cannot create the
        // parent dir on top of a real file, so the bind step has to
        // error out. The test asserts the error is surfaced as
        // `BootError::Socket` rather than being swallowed by the
        // spawned-task discard the old `run_server` used.
        let tmp = tempfile::tempdir().unwrap();
        let parent_as_file = tmp.path().join("not_a_dir");
        std::fs::write(&parent_as_file, b"this is a file, not a dir").unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: parent_as_file.join("cclaw.sock"),
            channels: Vec::new(),
            ..HostConfig::default()
        };
        let shutdown = CancellationToken::new();
        let rt: Box<dyn ContainerRuntime> = Box::new(crate::tests::NoopRuntime::default());
        let err = run_host(cfg, Some(rt), shutdown, None).await.unwrap_err();
        assert!(
            matches!(err, BootError::Socket(_)),
            "expected BootError::Socket, got {err:?}",
        );
    }

    #[tokio::test]
    async fn run_host_orphan_cleanup_failure_is_non_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            channels: Vec::new(),
            ..HostConfig::default()
        };
        let shutdown = CancellationToken::new();
        let rt: Box<dyn ContainerRuntime> = Box::new(
            crate::tests::NoopRuntime::default().fail_with(RtError::Unavailable("nope".into())),
        );
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_host(cfg, Some(rt), cancel, None).await.unwrap();
        });
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    // -----------------------------------------------------------------------
    // build_pending_notifier tests
    // -----------------------------------------------------------------------

    /// Helper: build a wired DB with one agent group and one messaging group.
    fn notifier_fixture() -> (
        copperclaw_db::central::CentralDb,
        copperclaw_types::AgentGroupId,
        copperclaw_types::MessagingGroupId,
        copperclaw_types::ChannelType,
        String, // platform_id
    ) {
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_db::tables::messaging_group_agents::{UpsertWiring, upsert as upsert_wire};
        use copperclaw_db::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
        use copperclaw_types::{ChannelType, EngageMode, SessionMode};

        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "notifier-test-ag".into(),
                folder: "nt".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let ct = ChannelType::new("telegram");
        let pid = "chat-notify".to_string();
        let mg = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ct.clone(),
                platform_id: pid.clone(),
                name: Some("Notify Group".into()),
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wire(
            &db,
            UpsertWiring {
                messaging_group_id: mg.id,
                agent_group_id: ag.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "known".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();
        (db, ag.id, mg.id, ct, pid)
    }

    #[test]
    fn notifier_dispatches_for_new_sender() {
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, SenderIdentity};

        let (db, ag_id, _mg_id, _ct, _pid) = notifier_fixture();
        let notifier = build_pending_notifier(db);

        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();

        let ctx = NewPendingCtx {
            sender: SenderIdentity {
                channel_type: ChannelType::new("slack"),
                identity: "U-new".into(),
                display_name: Some("New User".into()),
            },
            agent_group_id: ag_id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx, dispatcher);
        assert_eq!(mock.dispatched_count(), 1, "should dispatch a notification");
        let dispatched_msgs = mock.dispatched.lock().unwrap();
        let (target, msg) = &dispatched_msgs[0];
        // Target should be the telegram group wired to this agent group.
        assert_eq!(
            target.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        let text = msg.content.get("text").unwrap().as_str().unwrap();
        assert!(
            text.contains("cclaw approvals approve"),
            "notification must include approval command: {text}"
        );
        assert!(
            text.contains("U-new"),
            "notification must include sender identity: {text}"
        );
        // Check notification is plain ASCII (no emojis).
        assert!(
            text.is_ascii(),
            "notification text must be plain ASCII: {text}"
        );
    }

    #[test]
    fn notifier_skips_repeat_sender() {
        use copperclaw_db::tables::unregistered_senders::{
            UpsertUnregisteredSender, upsert as upsert_unreg,
        };
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, SenderIdentity};

        let (db, ag_id, _mg_id, _ct, _pid) = notifier_fixture();

        // Pre-populate unregistered_senders so the notifier thinks this
        // sender has already been seen (and notified) before.
        let ct = ChannelType::new("slack");
        let identity = "U-repeat".to_string();
        upsert_unreg(
            &db,
            UpsertUnregisteredSender {
                channel_type: ct.clone(),
                platform_id: identity.clone(),
                user_id: None,
                sender_name: None,
                reason: "scope_pending".into(),
                messaging_group_id: None,
                agent_group_id: None,
            },
        )
        .unwrap();

        let notifier = build_pending_notifier(db);
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();

        let ctx = NewPendingCtx {
            sender: SenderIdentity {
                channel_type: ct,
                identity,
                display_name: None,
            },
            agent_group_id: ag_id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx, dispatcher);
        assert_eq!(
            mock.dispatched_count(),
            0,
            "should NOT dispatch for a repeat sender"
        );
    }

    #[test]
    fn notifier_skips_when_no_messaging_group_wired() {
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, SenderIdentity};

        // Agent group with NO wired messaging group.
        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();
        let ag = create_ag(
            &db,
            CreateAgentGroup {
                name: "no-mg-ag".into(),
                folder: "nm".into(),
                agent_provider: None,
            },
        )
        .unwrap();

        let notifier = build_pending_notifier(db);
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();

        let ctx = NewPendingCtx {
            sender: SenderIdentity {
                channel_type: ChannelType::new("discord"),
                identity: "D-orphan".into(),
                display_name: None,
            },
            agent_group_id: ag.id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx, dispatcher);
        // Must silently skip — no dispatch, no panic.
        assert_eq!(mock.dispatched_count(), 0);
    }

    #[test]
    fn notifier_multiple_agent_groups_routes_independently() {
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_db::tables::messaging_group_agents::{UpsertWiring, upsert as upsert_wire};
        use copperclaw_db::tables::messaging_groups::{UpsertMessagingGroup, upsert as upsert_mg};
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, EngageMode, SenderIdentity, SessionMode};

        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();

        // Agent group A → slack channel.
        let ag_a = create_ag(
            &db,
            CreateAgentGroup {
                name: "ag-a".into(),
                folder: "a".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg_slack = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("slack"),
                platform_id: "C-slack".into(),
                name: None,
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wire(
            &db,
            UpsertWiring {
                messaging_group_id: mg_slack.id,
                agent_group_id: ag_a.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "known".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();

        // Agent group B → discord channel.
        let ag_b = create_ag(
            &db,
            CreateAgentGroup {
                name: "ag-b".into(),
                folder: "b".into(),
                agent_provider: None,
            },
        )
        .unwrap();
        let mg_discord = upsert_mg(
            &db,
            UpsertMessagingGroup {
                channel_type: ChannelType::new("discord"),
                platform_id: "C-discord".into(),
                name: None,
                is_group: true,
                unknown_sender_policy: "strict".into(),
            },
        )
        .unwrap();
        upsert_wire(
            &db,
            UpsertWiring {
                messaging_group_id: mg_discord.id,
                agent_group_id: ag_b.id,
                engage_mode: EngageMode::Mention,
                engage_pattern: None,
                sender_scope: "known".into(),
                ignored_message_policy: "drop".into(),
                session_mode: SessionMode::Shared,
                priority: 0,
            },
        )
        .unwrap();

        let notifier = build_pending_notifier(db);

        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();

        // Fire notifier for a sender targeting agent group A.
        let ctx_a = NewPendingCtx {
            sender: SenderIdentity {
                channel_type: ChannelType::new("gchat"),
                identity: "user-a".into(),
                display_name: None,
            },
            agent_group_id: ag_a.id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx_a, Arc::clone(&dispatcher));

        // Fire notifier for a different sender targeting agent group B.
        let ctx_b = NewPendingCtx {
            sender: SenderIdentity {
                channel_type: ChannelType::new("gchat"),
                identity: "user-b".into(),
                display_name: None,
            },
            agent_group_id: ag_b.id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx_b, dispatcher);

        let all_dispatched = mock.dispatched.lock().unwrap();
        assert_eq!(
            all_dispatched.len(),
            2,
            "each agent group gets its own notification"
        );
        let targets: Vec<_> = all_dispatched
            .iter()
            .map(|(t, _)| t.channel_type.as_ref().map_or("", ChannelType::as_str))
            .collect();
        assert!(targets.contains(&"slack"), "ag-a should notify via slack");
        assert!(
            targets.contains(&"discord"),
            "ag-b should notify via discord"
        );
    }

    #[tokio::test]
    async fn run_host_inits_cli_channel_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("cclaw.sock"),
            channels: vec![crate::config::ChannelInit {
                channel_type: "cli".into(),
                config: serde_json::json!({}),
            }],
            ..HostConfig::default()
        };
        let shutdown = CancellationToken::new();
        let rt: Box<dyn ContainerRuntime> = Box::new(crate::tests::NoopRuntime::default());
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_host(cfg, Some(rt), cancel, None).await.unwrap();
        });
        for _ in 0..80 {
            if tmp.path().join("channels").join("cli").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(tmp.path().join("channels").join("cli").exists());
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }
}
