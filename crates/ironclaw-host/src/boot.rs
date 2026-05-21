//! Host boot sequence — `ironclaw run` entry point.
//!
//! See `PLAN.md` § 6 T3. The steps in this module mirror the numbered list
//! there.

use crate::channels_init::{build_registry, init_channels, DEFAULT_INBOUND_BUFFER};
use crate::config::HostConfig;
use crate::context::HostContext;
use crate::orphans::cleanup_orphans;
use crate::sessions::FsSessionRoot;
use crate::socket::run_server;
use anyhow::Result;
use dashmap::DashMap;
use ironclaw_channels_core::ChannelAdapter;
use ironclaw_container_rt::{ContainerRuntime, RtError};
use ironclaw_db::central::CentralDb;
use ironclaw_db::migrate::{run_migrations, MigrationSet};
use ironclaw_host_delivery::DeliveryService;
use ironclaw_host_router::Router;
use ironclaw_host_sweep::SweepService;
use ironclaw_modules::{
    AgentToAgentModule, ApprovalsModule, InteractiveModule, Module, MountSecurityModule,
    NewPendingCtx, NewPendingNotifier, PermissionsModule, SchedulingModule, SelfModModule,
    TypingConfig, TypingModule,
};
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
    "Run: iclaw approvals approve --channel <channel_type> --identity <identity>",
);

/// Build the closure wired to [`ApprovalsModule::with_new_pending_notifier`].
///
/// The notifier fires synchronously inside the router's sender-scope gate on
/// every inbound from an unknown sender. It must be fast:
///
/// 1. Check [`ironclaw_db::tables::unregistered_senders`] — if a row already
///    exists, the operator was already notified (the router writes the row
///    after the gate returns, so a pre-existing row means the sender has been
///    seen before). Skip.
/// 2. Look up the messaging groups wired to the agent group. Take the first
///    one (ordered by priority desc, then creation time — consistent with the
///    router's own list). If none exist, log at info and return.
/// 3. Dispatch a text notification to that messaging group via the
///    [`ironclaw_modules::DeliveryDispatcher`].
///
/// The dispatcher call itself is synchronous (the host implementation spawns
/// the adapter work in the background), so the gate hot-path is unblocked.
fn build_pending_notifier(central: ironclaw_db::central::CentralDb) -> NewPendingNotifier {
    Arc::new(move |ctx: NewPendingCtx, dispatcher| {
        // De-dupe: if this sender has been seen before, a notification was
        // already posted. The `unregistered_senders` row is written by the
        // router AFTER the gate returns, so absence of the row means this is
        // the sender's first ever contact.
        let already_seen = ironclaw_db::tables::unregistered_senders::get(
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
        let wirings = match ironclaw_db::tables::messaging_group_agents::list_for_ag(
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
        let mg = match ironclaw_db::tables::messaging_groups::get(&central, wiring.messaging_group_id) {
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

        let target = ironclaw_modules::DispatchTarget::channel(
            mg.channel_type.clone(),
            mg.platform_id.clone(),
            None,
        );
        let message = ironclaw_types::OutboundMessage {
            kind: ironclaw_types::MessageKind::Chat,
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
#[derive(Debug, Error)]
pub enum BootError {
    /// Migrations could not be applied.
    #[error("central migrations failed: {0}")]
    Migrate(#[source] ironclaw_db::DbError),
    /// `ContainerRuntime` could not be detected.
    #[error("no container runtime detected: {0}")]
    RuntimeDetect(#[source] RtError),
    /// Opening the central DB failed.
    #[error("open central db failed: {0}")]
    OpenCentral(#[source] ironclaw_db::DbError),
    /// Socket server returned an unexpected I/O error before shutdown.
    #[error("socket server error: {0}")]
    Socket(#[source] std::io::Error),
}

impl BootError {
    /// Process exit code to use for this error.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Migrate(_) | Self::OpenCentral(_) => 2,
            Self::RuntimeDetect(_) => 3,
            Self::Socket(_) => 4,
        }
    }
}

/// Run-only-migrations entry point used by `ironclaw migrate`.
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
    adapters: DashMap<ironclaw_types::ChannelType, Arc<dyn ChannelAdapter>>,
) -> Result<HostState, BootError> {
    let path = cfg.central_db_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| BootError::OpenCentral(e.into()))?;
    }
    let central = CentralDb::open(&path).map_err(BootError::OpenCentral)?;
    let session_root = Arc::new(FsSessionRoot::new(cfg.sessions_root()));

    let router_root: Arc<dyn ironclaw_host_router::SessionRoot + Send + Sync> =
        Arc::new(FsSessionRoot::new(cfg.sessions_root()));
    let router = Arc::new(Router::new(central.clone(), router_root));

    let delivery_root: Arc<dyn ironclaw_host_delivery::SessionRoot> =
        Arc::new(FsSessionRoot::new(cfg.sessions_root()));
    let dispatcher_map: Arc<DashMap<ironclaw_types::ChannelType, Arc<dyn ChannelAdapter>>> =
        Arc::new(DashMap::new());
    for entry in &adapters {
        dispatcher_map.insert(entry.key().clone(), Arc::clone(entry.value()));
    }
    let resolver_map = Arc::clone(&dispatcher_map);
    let resolver: ironclaw_host_delivery::AdapterResolver = {
        let map = resolver_map;
        Arc::new(move |ct| map.get(ct).map(|r| r.clone()))
    };
    let dispatcher: Arc<dyn ironclaw_modules::DeliveryDispatcher> =
        Arc::new(ironclaw_host_delivery::HostDispatcher::new(resolver));
    let delivery = DeliveryService::new(central.clone(), delivery_root, adapters, dispatcher);

    let sweep_root: Arc<dyn ironclaw_host_sweep::SessionRoot> =
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
pub async fn install_modules(host_ctx: Arc<HostContext>) {
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(TypingModule::new(TypingConfig::default())),
        Box::new(MountSecurityModule::new()),
        Box::new(PermissionsModule::deny_all()),
        // Pre-approve the cli channel's deterministic `local` sender.
        // The cli channel reads the host's own stdin — the only
        // "sender" is the operator running `ironclaw run` already, so
        // there's no meaningful approval gate to apply. Without this
        // pre-seed, every interactive chat would silently deadlock on
        // a missing approval CLI surface.
        //
        // For every other sender, the gate's persistent fallback
        // queries the central `users` table — that's how `iclaw
        // approvals approve` lands without a host restart.
        Box::new(
            ApprovalsModule::with_initial_approved(vec![
                ironclaw_types::SenderIdentity {
                    channel_type: ironclaw_types::ChannelType::new(
                        ironclaw_types::ChannelType::CLI,
                    ),
                    identity: "local".to_string(),
                    display_name: Some("local".to_string()),
                },
            ])
            .with_persistent_lookup({
                let central = host_ctx.central().clone();
                std::sync::Arc::new(move |sender| {
                    let kind = sender.channel_type.as_str();
                    ironclaw_db::tables::users::get_by_identity(
                        &central,
                        kind,
                        &sender.identity,
                    )
                    .ok()
                    .flatten()
                    .is_some()
                })
            })
            .with_new_pending_notifier(build_pending_notifier(host_ctx.central().clone())),
        ),
        Box::new(InteractiveModule::default()),
        Box::new(SchedulingModule),
        Box::new(AgentToAgentModule),
        Box::new(SelfModModule),
    ];
    for m in modules {
        let name = m.name();
        let ctx: Arc<dyn ironclaw_modules::ModuleContext> = Arc::clone(&host_ctx)
            as Arc<dyn ironclaw_modules::ModuleContext>;
        if let Err(err) = m.install(ctx).await {
            warn!(module = name, ?err, "module install failed; continuing");
        }
    }
}

/// Full host entry point used by `ironclaw run`.
///
/// `runtime` may be provided by the caller (for tests); when `None` the
/// real runtime is detected via [`ironclaw_container_rt::detect`].
pub async fn run_host(
    cfg: HostConfig,
    runtime: Option<Box<dyn ContainerRuntime>>,
    shutdown: CancellationToken,
) -> Result<(), BootError> {
    info!(
        data_dir = %cfg.data_dir.display(),
        socket = %cfg.ncl_socket_path.display(),
        "ironclaw boot starting",
    );

    // 1-4. Migrations.
    run_migrations_only(&cfg)?;

    // 5. Detect container runtime. Wrap in an Arc so we can hand one
    // clone to the orphan-cleanup call and another to the container
    // manager later in this fn.
    let runtime: Arc<dyn ContainerRuntime> = match runtime {
        Some(r) => Arc::from(r),
        None => ironclaw_container_rt::detect()
            .await
            .map_err(BootError::RuntimeDetect)?
            .into(),
    };

    // 6. Orphan cleanup (best effort). Removes any leftover session
    // containers from a previous host process.
    if let Err(err) = cleanup_orphans(runtime.as_ref(), &cfg.install_slug).await {
        warn!(?err, "orphan cleanup failed; continuing boot");
    }

    // 6b. Optional Prometheus metrics endpoint. Reads IRONCLAW_METRICS_ADDR;
    // no-ops when unset. Warns on bind failure but does not abort boot.
    ironclaw_metrics::maybe_start_server(Some(shutdown.clone())).await;

    // 7. Build channel registry.
    let registry = build_registry();

    // 8. Init channels.
    let (inbound_tx, mut inbound_rx) = mpsc::channel(DEFAULT_INBOUND_BUFFER);
    let initialized =
        init_channels(&registry, &cfg.channels, inbound_tx, &cfg.data_dir).await;
    let adapters: DashMap<ironclaw_types::ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
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
    if let Ok(running) = ironclaw_db::tables::sessions::list_running(&state.central) {
        for s in &running {
            if let Err(err) =
                ironclaw_db::tables::sessions::mark_container_stopped(&state.central, s.id)
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

    // 10. Install modules.
    let host_ctx = HostContext::for_router(Arc::clone(&state.router), Arc::clone(&state.delivery));
    install_modules(Arc::clone(&host_ctx)).await;

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
    let active = tokio::spawn(
        Arc::clone(&state.delivery).run_active_loop(shutdown.clone()),
    );
    let sweep_delivery = tokio::spawn(
        Arc::clone(&state.delivery).run_sweep_loop(shutdown.clone()),
    );

    // 13. Sweep loop.
    let sweep_loop = tokio::spawn(Arc::clone(&state.sweep).run_loop(shutdown.clone()));

    let manager_task = spawn_container_manager(
        &cfg,
        state.central.clone(),
        Arc::clone(&runtime),
        shutdown.clone(),
    );

    // 14. Spawn socket server.
    let socket_path = cfg.ncl_socket_path.clone();
    let socket_central = state.central.clone();
    let socket_cancel = shutdown.clone();
    let socket_task = tokio::spawn(async move {
        run_server(socket_path, socket_central, socket_cancel).await
    });

    print_ready_banner(&cfg, &initialized);
    info!("ironclaw boot complete; idling");

    // 15. Idle until shutdown.
    wait_for_signal(shutdown.clone()).await;

    info!("shutdown requested; cancelling tasks");
    shutdown.cancel();

    // Await all tasks with a 30s deadline.
    let deadline = Duration::from_secs(30);
    let _ = tokio::time::timeout(deadline, async {
        let _ = consumer.await;
        let _ = active.await;
        let _ = sweep_delivery.await;
        let _ = sweep_loop.await;
        if let Some(t) = manager_task {
            let _ = t.await;
        }
        let _ = socket_task.await;
    })
    .await;

    Ok(())
}

/// Block until a SIGINT/SIGTERM is observed, or `shutdown` is cancelled
/// externally (whichever comes first).
pub async fn wait_for_signal(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
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
        tokio::select! {
            _ = sigint.recv() => info!("SIGINT received"),
            _ = sigterm.recv() => info!("SIGTERM received"),
            () = shutdown.cancelled() => info!("external cancellation"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = shutdown.cancelled().await;
    }
}

/// Path under which channels keep their per-instance data directory. Exposed
/// here so other crates can resolve the same location.
pub fn channel_data_dir(data_root: &std::path::Path, channel_type: &str) -> PathBuf {
    data_root.join("channels").join(channel_type)
}

/// Spawn the container-manager task. Returns `Some(handle)` when the
/// default image tag is known; otherwise logs a warning and returns
/// `None` so the host still boots (sessions just won't get a runner).
fn spawn_container_manager(
    cfg: &HostConfig,
    central: ironclaw_db::central::CentralDb,
    runtime: Arc<dyn ContainerRuntime>,
    shutdown: CancellationToken,
) -> Option<tokio::task::JoinHandle<()>> {
    let Some(image_tag) = cfg.default_image_tag.clone() else {
        warn!(
            "no IRONCLAW_DEFAULT_IMAGE_TAG configured; container manager disabled. \
             Sessions will accept inbound but no agent will respond. Run \
             `ironclaw-setup` to build the image and write the tag to .env."
        );
        return None;
    };
    let manager_cfg = crate::container_manager::ManagerConfig {
        install_slug: cfg.install_slug.clone(),
        // The router/delivery/sweep build SessionPaths via
        // `FsSessionRoot::new(cfg.sessions_root())`, which means the
        // actual on-disk root is `data_dir/sessions` (and
        // `SessionPaths::new` appends another `/sessions/<ag>/<session>`
        // on top of that). Match the same shape here so we open the
        // same inbound.db the rest of the host writes to.
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
        anthropic_api_key: std::env::var("ANTHROPIC_API_KEY").ok(),
        anthropic_base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
        idle_timeout_secs: crate::container_manager::DEFAULT_IDLE_TIMEOUT_SECS,
        heartbeat_stale_secs: crate::container_manager::DEFAULT_HEARTBEAT_STALE_SECS,
        stop_grace_secs: crate::container_manager::DEFAULT_STOP_GRACE_SECS,
        skills_dir: cfg.skills_dir.clone(),
        groups_dir: cfg.groups_dir.clone(),
    };
    let manager = Arc::new(crate::container_manager::ContainerManager::new(
        central,
        runtime,
        manager_cfg,
    ));
    Some(tokio::spawn(manager.run_loop(shutdown)))
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
        format!("ironclaw {} ready", env!("CARGO_PKG_VERSION")),
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
            BootError::Migrate(ironclaw_db::DbError::NotFound).exit_code(),
            2
        );
        assert_eq!(
            BootError::OpenCentral(ironclaw_db::DbError::NotFound).exit_code(),
            2
        );
        assert_eq!(
            BootError::RuntimeDetect(RtError::Unavailable("x".into())).exit_code(),
            3
        );
        assert_eq!(
            BootError::Socket(std::io::Error::other("x")).exit_code(),
            4
        );
    }

    #[test]
    fn boot_error_display_renders() {
        assert!(
            BootError::Migrate(ironclaw_db::DbError::NotFound)
                .to_string()
                .contains("migrations failed")
        );
        assert!(
            BootError::RuntimeDetect(RtError::Unavailable("x".into()))
                .to_string()
                .contains("no container runtime")
        );
    }

    #[test]
    fn ready_banner_includes_paths_and_no_channels_marker() {
        let cfg = HostConfig {
            data_dir: PathBuf::from("/srv/iron/data"),
            ncl_socket_path: PathBuf::from("/srv/iron/data/iclaw.sock"),
            ..HostConfig::default()
        };
        let lines = ready_banner_lines(&cfg, &[]);
        assert!(lines[0].contains("ironclaw"));
        assert!(lines.iter().any(|l| l.contains("/srv/iron/data")));
        assert!(lines.iter().any(|l| l.contains("iclaw.sock")));
        assert!(lines.iter().any(|l| l.contains("(none)")));
    }

    #[test]
    fn ready_banner_lists_channels() {
        use crate::channels_init::InitializedChannel;
        use ironclaw_channels_core::testing::MockAdapter;
        use ironclaw_types::ChannelType;
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
        let ctx = HostContext::for_router(
            Arc::clone(&state.router),
            Arc::clone(&state.delivery),
        );
        install_modules(ctx).await;
        // At least permissions+approvals install hooks; assert something
        // landed on the router's chain.
        assert!(
            state.router.hooks().has_access_gate()
                || state.router.hooks().has_sender_scope_gate()
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
            ncl_socket_path: tmp.path().join("iclaw.sock"),
            channels: Vec::new(), // no channels -> no per-channel scaffold
            ..HostConfig::default()
        };
        let shutdown = CancellationToken::new();
        let rt: Box<dyn ContainerRuntime> = Box::new(crate::tests::NoopRuntime::default());
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_host(cfg, Some(rt), cancel).await.unwrap();
        });
        // Wait briefly so the socket file appears.
        for _ in 0..80 {
            if tmp.path().join("iclaw.sock").exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(tmp.path().join("iclaw.sock").exists(), "socket should be up");
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn run_host_orphan_cleanup_failure_is_non_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("iclaw.sock"),
            channels: Vec::new(),
            ..HostConfig::default()
        };
        let shutdown = CancellationToken::new();
        let rt: Box<dyn ContainerRuntime> = Box::new(
            crate::tests::NoopRuntime::default()
                .fail_with(RtError::Unavailable("nope".into())),
        );
        let cancel = shutdown.clone();
        let task = tokio::spawn(async move {
            run_host(cfg, Some(rt), cancel).await.unwrap();
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
        ironclaw_db::central::CentralDb,
        ironclaw_types::AgentGroupId,
        ironclaw_types::MessagingGroupId,
        ironclaw_types::ChannelType,
        String, // platform_id
    ) {
        use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        use ironclaw_db::tables::messaging_group_agents::{upsert as upsert_wire, UpsertWiring};
        use ironclaw_db::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
        use ironclaw_types::{ChannelType, EngageMode, SessionMode};

        let db = ironclaw_db::central::CentralDb::open_in_memory().unwrap();
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
        use ironclaw_modules::context::MockDispatcher;
        use ironclaw_modules::DeliveryDispatcher;
        use ironclaw_types::{ChannelType, SenderIdentity};

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
            text.contains("iclaw approvals approve"),
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
        use ironclaw_db::tables::unregistered_senders::{upsert as upsert_unreg, UpsertUnregisteredSender};
        use ironclaw_modules::context::MockDispatcher;
        use ironclaw_modules::DeliveryDispatcher;
        use ironclaw_types::{ChannelType, SenderIdentity};

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
        use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        use ironclaw_modules::context::MockDispatcher;
        use ironclaw_modules::DeliveryDispatcher;
        use ironclaw_types::{ChannelType, SenderIdentity};

        // Agent group with NO wired messaging group.
        let db = ironclaw_db::central::CentralDb::open_in_memory().unwrap();
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
        use ironclaw_db::tables::agent_groups::{create as create_ag, CreateAgentGroup};
        use ironclaw_db::tables::messaging_group_agents::{upsert as upsert_wire, UpsertWiring};
        use ironclaw_db::tables::messaging_groups::{upsert as upsert_mg, UpsertMessagingGroup};
        use ironclaw_modules::context::MockDispatcher;
        use ironclaw_modules::DeliveryDispatcher;
        use ironclaw_types::{ChannelType, EngageMode, SenderIdentity, SessionMode};

        let db = ironclaw_db::central::CentralDb::open_in_memory().unwrap();

        // Agent group A → slack channel.
        let ag_a = create_ag(&db, CreateAgentGroup { name: "ag-a".into(), folder: "a".into(), agent_provider: None }).unwrap();
        let mg_slack = upsert_mg(&db, UpsertMessagingGroup {
            channel_type: ChannelType::new("slack"),
            platform_id: "C-slack".into(),
            name: None,
            is_group: true,
            unknown_sender_policy: "strict".into(),
        }).unwrap();
        upsert_wire(&db, UpsertWiring {
            messaging_group_id: mg_slack.id,
            agent_group_id: ag_a.id,
            engage_mode: EngageMode::Mention,
            engage_pattern: None,
            sender_scope: "known".into(),
            ignored_message_policy: "drop".into(),
            session_mode: SessionMode::Shared,
            priority: 0,
        }).unwrap();

        // Agent group B → discord channel.
        let ag_b = create_ag(&db, CreateAgentGroup { name: "ag-b".into(), folder: "b".into(), agent_provider: None }).unwrap();
        let mg_discord = upsert_mg(&db, UpsertMessagingGroup {
            channel_type: ChannelType::new("discord"),
            platform_id: "C-discord".into(),
            name: None,
            is_group: true,
            unknown_sender_policy: "strict".into(),
        }).unwrap();
        upsert_wire(&db, UpsertWiring {
            messaging_group_id: mg_discord.id,
            agent_group_id: ag_b.id,
            engage_mode: EngageMode::Mention,
            engage_pattern: None,
            sender_scope: "known".into(),
            ignored_message_policy: "drop".into(),
            session_mode: SessionMode::Shared,
            priority: 0,
        }).unwrap();

        let notifier = build_pending_notifier(db);

        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();

        // Fire notifier for a sender targeting agent group A.
        let ctx_a = NewPendingCtx {
            sender: SenderIdentity { channel_type: ChannelType::new("gchat"), identity: "user-a".into(), display_name: None },
            agent_group_id: ag_a.id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx_a, Arc::clone(&dispatcher));

        // Fire notifier for a different sender targeting agent group B.
        let ctx_b = NewPendingCtx {
            sender: SenderIdentity { channel_type: ChannelType::new("gchat"), identity: "user-b".into(), display_name: None },
            agent_group_id: ag_b.id,
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx_b, dispatcher);

        let all_dispatched = mock.dispatched.lock().unwrap();
        assert_eq!(all_dispatched.len(), 2, "each agent group gets its own notification");
        let targets: Vec<_> = all_dispatched
            .iter()
            .map(|(t, _)| t.channel_type.as_ref().map_or("", ChannelType::as_str))
            .collect();
        assert!(targets.contains(&"slack"), "ag-a should notify via slack");
        assert!(targets.contains(&"discord"), "ag-b should notify via discord");
    }

    #[tokio::test]
    async fn run_host_inits_cli_channel_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = HostConfig {
            data_dir: tmp.path().to_path_buf(),
            ncl_socket_path: tmp.path().join("iclaw.sock"),
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
            run_host(cfg, Some(rt), cancel).await.unwrap();
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
