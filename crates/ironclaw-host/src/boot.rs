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
    PermissionsModule, SchedulingModule, SelfModModule, TypingConfig, TypingModule,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

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
        Box::new(ApprovalsModule::new()),
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

    // 5. Detect container runtime.
    let runtime = match runtime {
        Some(r) => r,
        None => ironclaw_container_rt::detect()
            .await
            .map_err(BootError::RuntimeDetect)?,
    };

    // 6. Orphan cleanup (best effort).
    if let Err(err) = cleanup_orphans(runtime.as_ref(), &cfg.install_slug).await {
        warn!(?err, "orphan cleanup failed; continuing boot");
    }

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

    // 14. Spawn socket server.
    let socket_path = cfg.ncl_socket_path.clone();
    let socket_central = state.central.clone();
    let socket_cancel = shutdown.clone();
    let socket_task = tokio::spawn(async move {
        run_server(socket_path, socket_central, socket_cancel).await
    });

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
