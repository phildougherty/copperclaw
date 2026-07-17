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
    Module, MountHostContext, MountSecurityModule, NewPendingCtx, NewPendingNotifier,
    PermissionsModule, SchedulingModule, SelfModModule, TypingConfig, TypingModule,
    create_agent_users_table_check,
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

/// Notice text prefixed to a freshly minted DM pairing code. Plain ASCII
/// (project "no emojis" rule). Delivered back to the *sender* through the
/// same outbound delivery path adapters render for ordinary chat messages —
/// not a card variant.
const PAIRING_NOTICE: &str = concat!(
    "You are not yet approved to talk to this assistant.\n",
    "Share this one-time pairing code with the operator to be approved:",
);

/// Build the closure wired to [`ApprovalsModule::with_pairing_notifier`].
///
/// Fires synchronously inside the router's sender-scope gate the moment an
/// unknown sender is seen. It:
///
/// 1. De-dupes against `unregistered_senders` (same as the operator notifier)
///    so a sender that keeps messaging doesn't mint a fresh code on every
///    inbound — only the first contact mints.
/// 2. Mints an 8-char, 1h, rate-limited (3/channel) code via
///    [`copperclaw_db::tables::dm_pairing_codes::mint`]. A rate-limit hit is
///    logged and the inbound still lands in pending (the operator can still
///    approve out-of-band via `cclaw approvals approve`).
/// 3. Delivers the code text **back to the sender's own DM channel** with a
///    plain `MessageKind::Chat` `{"text": ...}` payload — the exact shape
///    every adapter renders today — via the [`DeliveryDispatcher`].
fn build_pairing_notifier(
    central: copperclaw_db::central::CentralDb,
) -> copperclaw_modules::PairingNotifier {
    Arc::new(move |ctx: NewPendingCtx, dispatcher| {
        // De-dupe on first contact only. The `unregistered_senders` row is
        // written by the router AFTER the gate returns, so absence means this
        // is the sender's first ever contact and the only time we mint.
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

        let minted = copperclaw_db::tables::dm_pairing_codes::mint(
            &central,
            copperclaw_db::tables::dm_pairing_codes::MintPairingCode {
                channel_type: ctx.sender.channel_type.clone(),
                identity: ctx.sender.identity.clone(),
                display_name: ctx.sender.display_name.clone(),
                agent_group_id: Some(ctx.agent_group_id),
                messaging_group_id: ctx.messaging_group_id,
            },
            chrono::Utc::now(),
        );
        let code = match minted {
            Ok(c) => c,
            Err(copperclaw_db::tables::dm_pairing_codes::MintError::RateLimited {
                channel_type,
                active,
            }) => {
                tracing::warn!(
                    channel_type = channel_type.as_str(),
                    active,
                    identity = ctx.sender.identity.as_str(),
                    "pairing: rate limit hit; not minting a new code for this sender"
                );
                return;
            }
            Err(err) => {
                tracing::warn!(
                    ?err,
                    identity = ctx.sender.identity.as_str(),
                    "pairing: failed to mint pairing code; skipping"
                );
                return;
            }
        };

        // Deliver the code back to the sender's own DM channel. The sender's
        // `identity` IS the platform id of their DM with the bot.
        let text = format!(
            "{notice}\n\n{code}\n\nThis code expires in 1 hour.",
            notice = PAIRING_NOTICE,
            code = code.code,
        );
        let target = copperclaw_modules::DispatchTarget::channel(
            ctx.sender.channel_type.clone(),
            ctx.sender.identity.clone(),
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
            "pairing: minted and delivered DM pairing code to sender"
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

/// Boot step 9b: reset stale `container_status=running` rows left
/// behind by the previous host process, and (M21 F3) tell the affected
/// users about it.
///
/// After the orphan cleanup the previous run's containers no longer
/// exist, but the sessions table may still claim they're alive. Without
/// the reset the container manager skips those sessions forever because
/// it only spawns for `container_status=stopped`.
///
/// The reset used to be silent — a user whose turn was in flight when
/// the host restarted watched the agent drop the turn with no
/// explanation, ever. Now each reset session runs through
/// [`crate::container_manager::classify::emit_boot_recovery_notice`],
/// which reuses the live crash-restart apology machinery: liveness-gated
/// (a clean idle restart — no pending inbound, or no in-flight
/// `processing_ack` claim — emits nothing) and deduped (one notice per
/// affected session per boot; the claim flip + tries stamp keep both a
/// repeat pass and the sweep's apology paths out). The pending inbound
/// itself is left untouched, so the respawned runner picks the turn
/// back up.
///
/// Every failure in here is logged and skipped — boot must not abort
/// over one session's broken per-session DB.
pub fn reset_stale_running_sessions(central: &CentralDb, sessions_root: &std::path::Path) {
    let running = match copperclaw_db::tables::sessions::list_running(central) {
        Ok(running) => running,
        Err(err) => {
            warn!(?err, "could not list running sessions for boot reset");
            return;
        }
    };
    let mut notices = 0usize;
    for s in &running {
        if let Err(err) = copperclaw_db::tables::sessions::mark_container_stopped(central, s.id) {
            warn!(session = %s.id.as_uuid(), ?err, "reset to stopped failed");
        }
        let paths =
            copperclaw_db::session::SessionPaths::new(sessions_root, s.agent_group_id, s.id);
        match crate::container_manager::classify::emit_boot_recovery_notice(&paths, s.id) {
            Ok(true) => notices += 1,
            Ok(false) => {}
            Err(err) => {
                warn!(
                    session = %s.id.as_uuid(),
                    ?err,
                    "boot recovery notice failed; continuing"
                );
            }
        }
    }
    if !running.is_empty() {
        info!(
            count = running.len(),
            recovery_notices = notices,
            "reset stale running sessions after orphan cleanup"
        );
    }
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
    // Mention gating policy is construction-time config on the Router (fixed
    // before it's shared behind an `Arc`, never mutated through it). The
    // secure default — require a mention in group chats, never in DMs — can
    // be overridden host-wide via env; per-group overrides come off each
    // wiring's `engage_mode` (Pattern engages on a regex match instead of a
    // mention; Mention / MentionSticky require one).
    let mention_gate = copperclaw_host_router::MentionGate {
        require_in_groups: parse_truthy_env_default("COPPERCLAW_REQUIRE_MENTION_GROUPS", true),
        require_in_dms: parse_truthy_env_default("COPPERCLAW_REQUIRE_MENTION_DMS", false),
    };
    let router =
        Arc::new(Router::new(central.clone(), router_root).with_mention_gate(mention_gate));

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

    // M18 G1: wire the in-chat approvals interceptor onto the router. It
    // recognises `approve:<id>` / `deny:<id>` button taps before the mention
    // gate, resolves them via the same DB path the CLI uses, and edits the
    // card via the same dispatcher the delivery loop uses. Registered here
    // (not through a module) because the closure needs both the central DB and
    // the delivery dispatcher, and the type lives in `copperclaw-modules` so
    // the router holds the hook slot without a circular dependency.
    router
        .hooks()
        .set_approval_interceptor(crate::approval_intercept::build_approval_interceptor(
            central.clone(),
            Arc::clone(&dispatcher),
        ));

    let delivery = DeliveryService::new(central.clone(), delivery_root, adapters, dispatcher);
    // M19 A4: give the delivery service the per-group data root so an approved
    // `save_skill` can write into `<groups_dir>/<ag>/skills`, which the next
    // spawn discovers.
    if let Some(groups_dir) = &cfg.groups_dir {
        delivery.set_groups_dir(groups_dir.clone());
    }

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
///
/// Returns a shared handle onto the installed [`InteractiveModule`]
/// (clones share pending-question state) so the caller can wire it into
/// the sweep's question-expiry check (M21 F2 — see
/// `SweepService::set_question_store`). Callers that don't run a sweep
/// (tests) can ignore the return value.
pub async fn install_modules(host_ctx: Arc<HostContext>, data_root: PathBuf) -> InteractiveModule {
    // Built outside the module list so a state-sharing clone survives
    // for the sweep seam; `Box::new(interactive.clone())` below installs
    // the same underlying pending-question state.
    let interactive = InteractiveModule::default();
    let modules: Vec<Box<dyn Module>> = vec![
        Box::new(TypingModule::new(TypingConfig::default())),
        // Register with a LIVE host root (the sessions dir all per-session
        // bind sources live under) so the module's `validate` enforces
        // against the real on-disk tree instead of the `host: None`
        // placeholder it shipped with — which made `validate` a no-op that
        // always returned `MountError::Empty`. The container manager's spawn
        // path holds its own equivalently-rooted validator; this registration
        // is what surfaces the module in `cclaw modules list` with a real
        // root and keeps the two in sync.
        Box::new(MountSecurityModule::with_host(MountHostContext {
            session_root: data_root.join("sessions"),
        })),
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
            .with_new_pending_notifier(build_pending_notifier(host_ctx.central().clone()))
            .with_pairing_notifier(build_pairing_notifier(host_ctx.central().clone())),
        ),
        Box::new(interactive.clone()),
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
        // The middle-tier `delegate` action shares the same spawn +
        // worktree machinery as `create_agent` (own container, writable
        // `sib/<id>` worktree) but keeps the spawned worker CONTAINED: no
        // channel wiring, reports only back to the parent. Gated by the
        // same `users_table_check` — spawning a write-capable container
        // is at least as privileged as `create_agent`. See M18 R7.
        Box::new(CreateAgentModule::new_delegate(
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
    interactive
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
    let (inbound_tx, inbound_rx) = mpsc::channel(DEFAULT_INBOUND_BUFFER);
    let initialized = init_channels(&registry, &cfg.channels, inbound_tx, &cfg.data_dir).await;
    let adapters: DashMap<copperclaw_types::ChannelType, Arc<dyn ChannelAdapter>> = DashMap::new();
    for ch in &initialized {
        adapters.insert(ch.channel_type.clone(), Arc::clone(&ch.adapter));
    }

    // 9. Assemble core services.
    let state = assemble(&cfg, adapters)?;

    // 9b. Reset stale `container_status=running` rows (and, M21 F3,
    // emit one recovery notice per session whose turn was in flight
    // when the previous host process died).
    reset_stale_running_sessions(&state.central, &cfg.sessions_root());

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

    // 10. Install modules. The returned handle shares the installed
    // InteractiveModule's pending-question state; wiring it into the
    // sweep turns on the question-expiry check (M21 F2) so an
    // unanswered `ask_user_question` past its TTL is surfaced out loud
    // instead of silently evaporating.
    let host_ctx = HostContext::for_router(Arc::clone(&state.router), Arc::clone(&state.delivery));
    let interactive = install_modules(Arc::clone(&host_ctx), cfg.data_dir.clone()).await;
    state.sweep.set_question_store(interactive);

    // 11-13c. Background loops, registered through the M21 S1 supervisor
    // (`crate::supervisor`) instead of bare `tokio::spawn`s: a panic (or an
    // unexpected return) in any of these used to silently kill that
    // subsystem for the remaining life of the process. The supervisor
    // restarts a dead loop on the decision-(e) backoff curve and exposes
    // per-loop liveness + restart counts via the `host.status` admin-socket
    // handler. Every loop still watches the same shutdown token it always
    // did — the supervisor only changes what happens when a loop dies
    // WITHOUT that token firing.
    let mut supervisor = crate::supervisor::Supervisor::new(shutdown.clone());

    // 10b. M21 O4 (decision (d)): opt-in operator-alert enqueuer. Reads the
    // `COPPERCLAW_OPERATOR_ALERT_*` env vars for a push destination; with none
    // configured it is disabled and produces ZERO new outbound (only the
    // existing log + metric). Shared, by Arc, between the supervisor
    // degraded-watch loop registered just below and the container manager
    // built later (crash-loop/OOM + spawn-failure-streak call sites). Uses the
    // live sessions root so its alert rows ride the same delivery pipeline the
    // rest of the host already drains.
    let operator_alerts = Arc::new(crate::operator_alerts::OperatorAlerts::from_env(
        state.central.clone(),
        cfg.sessions_root(),
    ));
    if operator_alerts.is_enabled() {
        info!("operator alerts enabled (COPPERCLAW_OPERATOR_ALERT_* configured)");
    } else {
        info!(
            "operator alerts disabled (set COPPERCLAW_OPERATOR_ALERT_CHANNEL + \
             COPPERCLAW_OPERATOR_ALERT_TARGET to enable); no new outbound will be produced"
        );
    }

    // 10b-ii. M21 O4 out-of-lane call sites (coordinator wiring): inject the
    // alerter into the sweep so the O2 quarantine event fires a critical alert
    // and the `checks::apology` copy can conditionally restore the truthful
    // "the operator has been notified" line. Set-once, mirrors
    // `set_stuck_actuator`; with no destination configured every sweep-side
    // call is a no-op (the pre-O4 behaviour).
    state.sweep.set_operator_alerts(
        Arc::clone(&operator_alerts) as Arc<dyn copperclaw_host_sweep::OperatorAlertSink>
    );

    // 10c. Supervisor permanent-failure → operator alert (M21 O4 hooking the
    // S1 degraded-watch seam). Registered as a supervised loop so a panic here
    // restarts on the same backoff curve as every other loop; each (re)start
    // mints a fresh `degraded_watch()` receiver. Fires exactly one Critical
    // alert when the supervisor-wide degraded flag flips to `true`.
    {
        let alerts = Arc::clone(&operator_alerts);
        let status = supervisor.status();
        let sd = shutdown.clone();
        supervisor.register("operator_alert_watch", move || {
            let alerts = Arc::clone(&alerts);
            let rx = status.degraded_watch();
            let sd = sd.clone();
            async move { alerts.run_degraded_watch(rx, sd).await }
        });
    }

    // 11. Inbound consumer. The receiver survives restarts behind a shared
    // Mutex: each incarnation locks it for its lifetime (a panic releases
    // the lock through unwinding), so a restarted consumer resumes the same
    // inbound stream with nothing lost.
    let inbound_rx = Arc::new(tokio::sync::Mutex::new(inbound_rx));
    {
        let router = Arc::clone(&state.router);
        let sd = shutdown.clone();
        supervisor.register("inbound_consumer", move || {
            let router = Arc::clone(&router);
            let rx = Arc::clone(&inbound_rx);
            let sd = sd.clone();
            async move {
                let mut inbound_rx = rx.lock().await;
                loop {
                    tokio::select! {
                        () = sd.cancelled() => break,
                        event = inbound_rx.recv() => {
                            let Some(event) = event else { break; };
                            if let Err(err) = router.route(event).await {
                                warn!(?err, "router::route failed");
                            }
                        }
                    }
                }
            }
        });
    }

    // 12. Delivery loops.
    {
        let delivery = Arc::clone(&state.delivery);
        let sd = shutdown.clone();
        supervisor.register("delivery_active", move || {
            Arc::clone(&delivery).run_active_loop(sd.clone())
        });
        let delivery = Arc::clone(&state.delivery);
        let sd = shutdown.clone();
        supervisor.register("delivery_sweep", move || {
            Arc::clone(&delivery).run_sweep_loop(sd.clone())
        });
    }

    // 13. Sweep loop.
    {
        let sweep = Arc::clone(&state.sweep);
        let sd = shutdown.clone();
        supervisor.register("sweep", move || Arc::clone(&sweep).run_loop(sd.clone()));
    }

    // 13b. Typing ticker. Keeps the channel's "agent is working"
    // indicator visible every 4 sec for any session with an active
    // container — fills the gap where `TypingModule` only fires on
    // inbound traffic, so users see a continuous bubble during long
    // tool loops rather than a 5-second flash then silence.
    //
    // M21 F1 (decision (c)): the ticker also holds the cold-start
    // spawn-activity registry the container manager (built below) writes
    // its in-flight spawn attempts into — so a first message to a fresh
    // session pulses typing during the whole spawn (image build, boot,
    // runner handshake) instead of dead air until the runner is up.
    let spawn_activity = Arc::new(crate::container_manager::SpawnActivity::new());
    let typing_ticker = Arc::new(
        crate::typing_ticker::TypingTicker::new(
            state.central.clone(),
            state.delivery.dispatcher(),
            cfg.data_dir.clone(),
        )
        .with_spawn_activity(Arc::clone(&spawn_activity)),
    );
    {
        let ticker = Arc::clone(&typing_ticker);
        let sd = shutdown.clone();
        supervisor.register("typing_ticker", move || {
            Arc::clone(&ticker).run_loop(sd.clone())
        });
    }

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
    {
        let watcher = Arc::clone(&todo_watcher);
        let sd = shutdown.clone();
        supervisor.register("todo_watcher", move || {
            Arc::clone(&watcher).run_loop(sd.clone())
        });
    }

    // Start the supervised loops + driver. `supervised` resolves only
    // after shutdown once every registered loop has drained; the status
    // Arc feeds the `host.status` handler on the admin socket below.
    let supervisor_status = supervisor.status();
    let supervised = supervisor.run();

    // 13b. Credential broker (Phase 0b). Opt-in via
    // `COPPERCLAW_CREDENTIAL_BROKER`. When enabled, this binds a loopback
    // model proxy holding the real key, and the container manager stops
    // forwarding the master key into containers (it mints per-session tokens
    // instead). Default-off, so the legacy spawn path is unchanged.
    let broker_default_provider = cfg
        .default_provider
        .clone()
        .unwrap_or_else(|| "anthropic".into());
    let broker = maybe_start_broker(
        state.central.clone(),
        &broker_default_provider,
        shutdown.clone(),
    )
    .await;

    // 13b-pre. M17 session-preview manager: the broker behind the agent's
    // `expose_preview` / `close_preview` tools. Wired into the delivery
    // service (which routes the reserved `__preview` relay requests to it)
    // and into the container manager (which tears a session's previews down
    // when its container stops). Its idle reaper runs under the same
    // shutdown token as every other loop.
    let preview = crate::preview::PreviewManager::new(state.central.clone(), Arc::clone(&runtime));
    preview.spawn_reaper(shutdown.clone());
    state
        .delivery
        .set_preview_broker(Arc::clone(&preview) as Arc<dyn copperclaw_modules::PreviewBroker>);
    // M18 V2: wire the delivery dispatcher so a `PreviewError::Disabled` raises
    // a one-tap "Enable previews for this group" approval card instead of a
    // dead error. The tap routes through the G1 in-chat approvals interceptor.
    preview.set_approval_dispatcher(state.delivery.dispatcher());

    // M19 A3: activate the merged V5 public-tunnel module. `make_preview_public`
    // relays through the same reserved `__preview` path as `expose_preview`, but
    // routes to this broker, which fronts a live preview's host port with an
    // operator-provided cloudflared tunnel — approval-gated end to end. OFF by
    // default: the host env master switch `COPPERCLAW_PUBLIC_TUNNEL_ENABLED`
    // (combined with the group's per-group `preview_enabled`) must be set, and
    // every exposure still needs an explicit operator approval. The tunnel
    // broker is also handed to the preview manager so closing / idle-reaping a
    // preview tears its public tunnel down with it (no public tunnel outlives
    // the app it fronted).
    let public_tunnel_enabled = parse_truthy_env("COPPERCLAW_PUBLIC_TUNNEL_ENABLED");
    let tunnel_provider: Arc<dyn copperclaw_modules::TunnelProvider> =
        Arc::new(copperclaw_modules::CloudflaredProvider::new());
    let tunnel_broker =
        copperclaw_modules::TunnelBroker::new(state.central.clone(), tunnel_provider);
    preview.set_tunnel_broker(Arc::clone(&tunnel_broker));
    let public_tunnel = crate::preview::PublicPreviewTunnel::new(
        Arc::clone(&preview),
        Arc::clone(&tunnel_broker),
        state.central.clone(),
        public_tunnel_enabled,
    );
    state
        .delivery
        .set_tunnel_broker(public_tunnel as Arc<dyn copperclaw_modules::PublicTunnelBroker>);

    let spawned = spawn_container_manager(
        &cfg,
        state.central.clone(),
        Arc::clone(&runtime),
        shutdown.clone(),
        Arc::clone(state.sweep.spawn_tracker()),
        broker,
        Arc::clone(&preview),
        // Event-driven wake: the router signals this handle after every
        // messages_in insert; the manager's reconcile loop ticks on it
        // immediately instead of waiting out the poll interval. Router and
        // manager live in the same host process, so this is a plain
        // in-process Notify — polling stays as the crash-safe fallback.
        state.router.inbound_wake(),
        // M21 F1: the same spawn-activity registry the typing ticker
        // reads, so mid-spawn sessions pulse typing from message one.
        Arc::clone(&spawn_activity),
        // M21 O4: the same operator-alert enqueuer the degraded-watch loop
        // uses, so the crash-loop/OOM + spawn-failure-streak thresholds push
        // to the configured operator destination.
        Arc::clone(&operator_alerts),
    );
    let (manager_task, manager_handle): (
        Option<tokio::task::JoinHandle<()>>,
        Option<Arc<crate::container_manager::ContainerManager>>,
    ) = match spawned {
        Some((task, mgr)) => (Some(task), Some(mgr)),
        None => (None, None),
    };

    // 13c-pre. M21 S2 (decision (a)): hand the container manager to the
    // sweep as its stuck-tool actuator. The sweep detects tools past the
    // absolute ceiling from per-session DB state; the manager owns
    // container lifecycle, so each detection is actuated through the
    // manager's `ReconcileAction::StuckRestart` rather than the sweep
    // touching the runtime directly (single-writer ownership). Wired
    // here — after the sweep loop is already registered — because the
    // manager is built later in the boot sequence; safe, since the
    // sweep's first pass fires a full SWEEP_POLL_MS after boot. A host
    // booted without a manager (no image tag) keeps ceiling detections
    // observe-only, exactly the pre-S2 behaviour.
    if let Some(mgr) = manager_handle.as_ref() {
        state
            .sweep
            .set_stuck_actuator(Arc::clone(mgr) as Arc<dyn copperclaw_host_sweep::StuckActuator>);
    }

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
    // Absolute data dir for handlers that touch per-session files
    // (`sessions.delete` dir removal, dead-letter replay). The daemon's
    // CWD is not the install root, so the HandlerCtx default relative
    // path must never be used here.
    let socket_data_dir = cfg.data_dir.clone();
    let socket_cancel = shutdown.clone();
    let socket_supervisor = Arc::clone(&supervisor_status);
    let listener = bind_listener(&socket_path).map_err(BootError::Socket)?;
    let socket_task = tokio::spawn(async move {
        serve_listener(
            listener,
            socket_path,
            socket_central,
            socket_data_dir,
            Some(socket_supervisor),
            socket_cancel,
        )
        .await
    });

    print_ready_banner(&cfg, &initialized);
    info!("copperclaw boot complete; idling");

    // 15. Idle until shutdown. SIGHUP triggers a secret-rotation
    // reload on the container manager (when one is spawned) and
    // resumes waiting; only SIGINT/SIGTERM/external-cancel exit.
    wait_for_signal_or_sighup(shutdown.clone(), manager_handle.clone(), env_file).await;

    info!("shutdown requested; cancelling tasks");
    shutdown.cancel();

    // Await all tasks with a 30s deadline. The supervisor's driver handle
    // resolves once every supervised loop (inbound consumer, both delivery
    // loops, sweep loop, typing ticker, todo watcher) has drained — the
    // same set, on the same token, inside the same deadline as before.
    let deadline = Duration::from_secs(30);
    let _ = tokio::time::timeout(deadline, async {
        let _ = supervised.await;
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
// Boot wiring: one parameter per optional subsystem the manager attaches
// (spawn tracker, broker, preview, wake). Splitting the fn or bundling the
// args into a struct would only move the wiring noise around.
#[allow(clippy::too_many_arguments)]
fn spawn_container_manager(
    cfg: &HostConfig,
    central: copperclaw_db::central::CentralDb,
    runtime: Arc<dyn ContainerRuntime>,
    shutdown: CancellationToken,
    spawn_tracker: Arc<copperclaw_host_sweep::SpawnAttemptTracker>,
    broker: Option<(Arc<crate::container_manager::broker::BrokerState>, String)>,
    preview: Arc<crate::preview::PreviewManager>,
    inbound_wake: Arc<tokio::sync::Notify>,
    spawn_activity: Arc<crate::container_manager::SpawnActivity>,
    operator_alerts: Arc<crate::operator_alerts::OperatorAlerts>,
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
        // Phase 0a v1 egress posture (Top 10 #6). Opt-in: default allow-all
        // keeps the spawn path unchanged; an operator sets
        // `COPPERCLAW_EGRESS_MODE=deny-default` to enable. The model endpoint
        // is always auto-injected into the resolved allow-list at spawn so
        // deny-default can never blackhole model traffic.
        egress_mode: crate::container_manager::parse_egress_mode(
            std::env::var("COPPERCLAW_EGRESS_MODE").ok().as_deref(),
        ),
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
    let mut manager =
        crate::container_manager::ContainerManager::new(central, runtime, manager_cfg)
            .with_spawn_tracker(spawn_tracker)
            .with_preview(preview)
            .with_wake_notify(inbound_wake)
            .with_spawn_activity(spawn_activity)
            .with_operator_alerts(operator_alerts);
    if let Some((broker_state, broker_base_url)) = broker {
        manager = manager.with_broker(broker_state, broker_base_url);
    }
    let manager = Arc::new(manager);
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
            // Phase 6 supply-chain: boot-time attestation digest check. Opt-in
            // via COPPERCLAW_EXPECTED_IMAGE_DIGEST — when an operator pins the
            // expected image content digest, compare the live digest against it
            // (a real comparison, not a stub). Default install pins nothing, so
            // this reports `no-baseline` and changes nothing. The result is
            // logged inside the helper; a mismatch is loud but does NOT degrade
            // boot (the image health check above already gates spawnability).
            let expected = std::env::var(crate::image_health::EXPECTED_IMAGE_DIGEST_ENV).ok();
            crate::image_health::check_boot_image_digest(&probe, image_tag, expected.as_deref())
                .await;
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

/// Like [`parse_truthy_env`] but with an explicit default when the variable
/// is unset/empty, and recognising falsey spellings so an operator can turn a
/// default-on knob OFF. Unrecognised non-empty values keep `default`.
fn parse_truthy_env_default(name: &str, default: bool) -> bool {
    match std::env::var(name).ok().as_deref() {
        Some("1" | "true" | "True" | "TRUE" | "yes" | "Yes" | "YES" | "on" | "On" | "ON") => true,
        Some("0" | "false" | "False" | "FALSE" | "no" | "No" | "NO" | "off" | "Off" | "OFF") => {
            false
        }
        // Unset, empty, or an unrecognised value keeps the default.
        None | Some(_) => default,
    }
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
        // NOTE: the old `COPPERCLAW_TOOL_BREADCRUMBS` /
        // `COPPERCLAW_BREADCRUMB_STYLE` forwards were removed with the
        // M18 Task HUD (card H1): per-tool breadcrumb chips no longer
        // exist, and the HUD's `COPPERCLAW_HUD_MODE` knob reaches the
        // runner through `runner.json`'s `hud_mode` field instead of
        // container env forwarding.
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

/// Resolve the credential broker (Phase 0b) from the process env and, when
/// enabled, bind the loopback listener and start its server task.
///
/// Returns `Some((state, base_url))` only when `COPPERCLAW_CREDENTIAL_BROKER`
/// is truthy AND a real `ANTHROPIC_API_KEY` is present (without a key there is
/// nothing to broker). The returned pair is handed to the container manager
/// via [`crate::container_manager::ContainerManager::with_broker`]; the spawn
/// path then stops forwarding the master key and mints per-session tokens
/// instead. Returns `None` (default, behaviour unchanged) otherwise.
///
/// Binds on loopback only. The bound port is dynamic (`:0`) so multiple hosts
/// on one box don't collide. A bind failure logs a warning and disables the
/// broker rather than killing boot — the host falls back to the default path.
async fn maybe_start_broker(
    central: copperclaw_db::central::CentralDb,
    default_provider: &str,
    shutdown: CancellationToken,
) -> Option<(Arc<crate::container_manager::broker::BrokerState>, String)> {
    use crate::container_manager::broker::{BrokerConfig, BrokerState};

    let enabled = BrokerConfig::parse_enabled(
        std::env::var("COPPERCLAW_CREDENTIAL_BROKER")
            .ok()
            .as_deref(),
    );
    let upstream_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let upstream_base = std::env::var("ANTHROPIC_BASE_URL").ok();
    let ttl_override = std::env::var("COPPERCLAW_BROKER_TOKEN_TTL_SECS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());

    let config = BrokerConfig::resolve(
        enabled,
        upstream_key.as_deref(),
        upstream_base.as_deref(),
        ttl_override,
    )?;
    if enabled && upstream_key.as_deref().filter(|k| !k.is_empty()).is_none() {
        warn!(
            "COPPERCLAW_CREDENTIAL_BROKER is set but ANTHROPIC_API_KEY is empty; \
             broker disabled, falling back to default behaviour"
        );
        return None;
    }

    let broker = Arc::new(BrokerState::new(config));
    let server_state = crate::container_manager::broker_server::production_state(
        Arc::clone(&broker),
        Some(default_provider),
        central,
    );

    // Bind loopback with a dynamic port so collisions never block boot.
    let bind_addr: std::net::SocketAddr = "127.0.0.1:0".parse().expect("loopback addr is valid");
    match crate::container_manager::broker_server::serve(bind_addr, server_state, shutdown).await {
        Ok(local) => {
            let base_url = format!("http://{local}");
            info!(
                addr = %local,
                "credential broker enabled; the master provider key will NOT be forwarded into containers"
            );
            Some((broker, base_url))
        }
        Err(err) => {
            warn!(
                error = %err,
                "credential broker failed to bind loopback listener; falling back to default key forwarding"
            );
            None
        }
    }
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

    // -----------------------------------------------------------------------
    // build_pairing_notifier tests
    // -----------------------------------------------------------------------

    #[test]
    fn pairing_notifier_mints_and_delivers_code_to_sender() {
        use copperclaw_db::tables::dm_pairing_codes;
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, SenderIdentity};

        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();
        let notifier = build_pairing_notifier(db.clone());

        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        let ctx = NewPendingCtx {
            sender: SenderIdentity {
                channel_type: ChannelType::new("telegram"),
                identity: "u-pair".into(),
                display_name: Some("Alice".into()),
            },
            agent_group_id: copperclaw_types::AgentGroupId::new(),
            messaging_group_id: None,
            first_seen: chrono::Utc::now(),
        };
        notifier(ctx, dispatcher);

        // A real row was persisted.
        let codes = dm_pairing_codes::list(&db, None).unwrap();
        assert_eq!(codes.len(), 1, "exactly one code minted");
        let minted = &codes[0];
        assert_eq!(minted.code.len(), 8, "code is 8 chars");
        assert_eq!(minted.identity, "u-pair");

        // The code was delivered back to the SENDER's own DM channel as a
        // plain Chat {"text": ...} message — the shape adapters render.
        assert_eq!(mock.dispatched_count(), 1);
        let sent = mock.dispatched.lock().unwrap();
        let (target, msg) = &sent[0];
        assert_eq!(
            target.channel_type.as_ref().map(ChannelType::as_str),
            Some("telegram")
        );
        assert_eq!(target.platform_id.as_deref(), Some("u-pair"));
        assert_eq!(msg.kind, copperclaw_types::MessageKind::Chat);
        let text = msg.content.get("text").unwrap().as_str().unwrap();
        assert!(
            text.contains(&minted.code),
            "delivered text carries the code"
        );
        assert!(text.is_ascii(), "no emojis: {text}");
    }

    #[test]
    fn pairing_notifier_skips_repeat_sender() {
        use copperclaw_db::tables::dm_pairing_codes;
        use copperclaw_db::tables::unregistered_senders::{
            UpsertUnregisteredSender, upsert as upsert_unreg,
        };
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, SenderIdentity};

        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();
        let ct = ChannelType::new("slack");
        // Mark the sender as already seen so the notifier de-dupes.
        upsert_unreg(
            &db,
            UpsertUnregisteredSender {
                channel_type: ct.clone(),
                platform_id: "U-repeat".into(),
                user_id: None,
                sender_name: None,
                reason: "scope_pending".into(),
                messaging_group_id: None,
                agent_group_id: None,
            },
        )
        .unwrap();

        let notifier = build_pairing_notifier(db.clone());
        let mock = MockDispatcher::new();
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        notifier(
            NewPendingCtx {
                sender: SenderIdentity {
                    channel_type: ct,
                    identity: "U-repeat".into(),
                    display_name: None,
                },
                agent_group_id: copperclaw_types::AgentGroupId::new(),
                messaging_group_id: None,
                first_seen: chrono::Utc::now(),
            },
            dispatcher,
        );
        assert_eq!(mock.dispatched_count(), 0, "repeat sender: no delivery");
        assert!(
            dm_pairing_codes::list(&db, None).unwrap().is_empty(),
            "repeat sender: no code minted"
        );
    }

    #[test]
    fn pairing_notifier_honours_rate_limit_per_channel() {
        use copperclaw_db::tables::dm_pairing_codes::{self, MAX_ACTIVE_CODES_PER_CHANNEL};
        use copperclaw_modules::DeliveryDispatcher;
        use copperclaw_modules::context::MockDispatcher;
        use copperclaw_types::{ChannelType, SenderIdentity};

        let db = copperclaw_db::central::CentralDb::open_in_memory().unwrap();
        let notifier = build_pairing_notifier(db.clone());
        let mock = MockDispatcher::new();

        // Fill the telegram channel to its cap via the notifier itself
        // (distinct first-contact senders each mint one code).
        for i in 0..MAX_ACTIVE_CODES_PER_CHANNEL {
            let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
            notifier(
                NewPendingCtx {
                    sender: SenderIdentity {
                        channel_type: ChannelType::new("telegram"),
                        identity: format!("u-{i}"),
                        display_name: None,
                    },
                    agent_group_id: copperclaw_types::AgentGroupId::new(),
                    messaging_group_id: None,
                    first_seen: chrono::Utc::now(),
                },
                dispatcher,
            );
        }
        assert_eq!(mock.dispatched_count(), MAX_ACTIVE_CODES_PER_CHANNEL);

        // One more telegram sender: rate-limited, so no mint and no delivery.
        let dispatcher: Arc<dyn DeliveryDispatcher> = mock.clone();
        notifier(
            NewPendingCtx {
                sender: SenderIdentity {
                    channel_type: ChannelType::new("telegram"),
                    identity: "u-overflow".into(),
                    display_name: None,
                },
                agent_group_id: copperclaw_types::AgentGroupId::new(),
                messaging_group_id: None,
                first_seen: chrono::Utc::now(),
            },
            dispatcher,
        );
        assert_eq!(
            mock.dispatched_count(),
            MAX_ACTIVE_CODES_PER_CHANNEL,
            "rate-limited sender must not deliver a new code"
        );
        let active =
            dm_pairing_codes::list(&db, Some(dm_pairing_codes::PairingStatus::Active)).unwrap();
        assert_eq!(active.len(), MAX_ACTIVE_CODES_PER_CHANNEL);
    }

    // -----------------------------------------------------------------------
    // M21 F3: boot-restart recovery notice (reset_stale_running_sessions)
    // -----------------------------------------------------------------------

    mod boot_recovery {
        use super::*;
        use crate::container_manager::classify::CRASH_RESTART_APOLOGY_TEXT;
        use crate::container_manager::config::{ManagerConfig, SkillsMode};
        use crate::container_manager::spawn::{
            DEFAULT_HEARTBEAT_STALE_SECS, DEFAULT_IDLE_TIMEOUT_SECS, DEFAULT_STOP_GRACE_SECS,
        };
        use crate::container_manager::{ContainerManager, ReconcileAction};
        use copperclaw_db::session::{SessionPaths, open_inbound, open_outbound};
        use copperclaw_db::tables::agent_groups::{CreateAgentGroup, create as create_ag};
        use copperclaw_db::tables::sessions::{self, CreateSession, create as create_session};
        use copperclaw_db::tables::{messages_in, messages_out, processing_ack};
        use copperclaw_types::{ChannelType, ContainerStatus, MessageId, MessageKind, Session};

        fn fixture_running_session(central: &CentralDb) -> Session {
            let ag = create_ag(
                central,
                CreateAgentGroup {
                    name: "recovery".into(),
                    folder: "recovery".into(),
                    agent_provider: None,
                },
            )
            .unwrap();
            let session = create_session(
                central,
                CreateSession {
                    agent_group_id: ag.id,
                    messaging_group_id: None,
                    thread_id: None,
                    agent_provider: None,
                    source_session_id: None,
                },
            )
            .unwrap();
            sessions::mark_container_running(central, session.id).unwrap();
            sessions::get(central, session.id).unwrap()
        }

        /// Seed one chat-routed pending inbound, `age` old, and return
        /// its id.
        fn seed_routed_inbound(
            paths: &SessionPaths,
            text: &str,
            age: chrono::Duration,
        ) -> MessageId {
            let msg_id = MessageId::new();
            let conn = open_inbound(paths).unwrap();
            messages_in::insert(
                &conn,
                &messages_in::WriteInbound {
                    id: msg_id,
                    kind: MessageKind::Chat,
                    timestamp: chrono::Utc::now() - age,
                    content: serde_json::json!({ "text": text }),
                    trigger: true,
                    on_wake: false,
                    process_after: None,
                    recurrence: None,
                    series_id: None,
                    platform_id: Some("tg-42".into()),
                    channel_type: Some(ChannelType::new("telegram")),
                    thread_id: Some("thread-7".into()),
                    source_session_id: None,
                    reply_to: None,
                    is_group: None,
                },
            )
            .unwrap();
            msg_id
        }

        /// Mark the inbound as picked up by a runner — the in-flight
        /// shape a host restart interrupts.
        fn claim_processing(paths: &SessionPaths, msg_id: MessageId) {
            let outbound = open_outbound(paths).unwrap();
            processing_ack::insert(
                &outbound,
                msg_id,
                processing_ack::ProcessingStatus::Processing,
            )
            .unwrap();
        }

        fn chat_rows(paths: &SessionPaths) -> Vec<copperclaw_types::MessageOutRow> {
            let outbound = open_outbound(paths).unwrap();
            messages_out::list_due(&outbound)
                .unwrap()
                .into_iter()
                .filter(|r| r.kind == MessageKind::Chat)
                .collect()
        }

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

        /// The F3 integration acceptance, happy path: a host restart
        /// with a turn in flight (session `running`, pending inbound,
        /// `processing_ack = processing`) emits exactly one recovery
        /// notice — the live crash path's copy, routed at the inbound —
        /// and the re-queued inbound still processes: it stays due, and
        /// the manager classifies the reset session as `Spawn` with no
        /// backoff (a host restart is not a container crash).
        #[tokio::test(start_paused = true)]
        async fn restart_with_turn_in_flight_emits_one_notice_and_requeues() {
            let tmp = tempfile::tempdir().unwrap();
            let central = CentralDb::open_in_memory().unwrap();
            let session = fixture_running_session(&central);
            let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
            paths.ensure_dirs().unwrap();
            let msg_id = seed_routed_inbound(&paths, "build the thing", chrono::Duration::zero());
            claim_processing(&paths, msg_id);

            reset_stale_running_sessions(&central, tmp.path());

            // Reset landed: the session is spawnable again.
            let updated = sessions::get(&central, session.id).unwrap();
            assert!(matches!(updated.container_status, ContainerStatus::Stopped));

            // Exactly one recovery notice, reusing the crash-restart
            // copy (no duplicated string), routed at the in-flight
            // inbound.
            let rows = chat_rows(&paths);
            assert_eq!(rows.len(), 1, "exactly one recovery notice");
            let notice = &rows[0];
            assert_eq!(notice.in_reply_to, Some(msg_id));
            assert_eq!(
                notice.channel_type.as_ref().map(ChannelType::as_str),
                Some("telegram")
            );
            assert_eq!(notice.platform_id.as_deref(), Some("tg-42"));
            assert_eq!(notice.thread_id.as_deref(), Some("thread-7"));
            assert_eq!(
                notice
                    .content
                    .get("text")
                    .and_then(serde_json::Value::as_str),
                Some(CRASH_RESTART_APOLOGY_TEXT),
                "the notice must reuse the crash-restart apology text"
            );

            // The claim is Failed (the boot path owns the turn now) but
            // the inbound row is still due, so the respawned runner
            // picks it back up.
            let outbound = open_outbound(&paths).unwrap();
            let claim = processing_ack::get(&outbound, msg_id).unwrap().unwrap();
            assert_eq!(claim.status, processing_ack::ProcessingStatus::Failed);
            let inbound = open_inbound(&paths).unwrap();
            assert_eq!(messages_in::count_due(&inbound).unwrap(), 1);

            // And the manager will actually respawn: no crash-loop
            // backoff applies to a host restart.
            let mgr = ContainerManager::new(
                central.clone(),
                Arc::new(crate::tests::NoopRuntime::default()),
                manager_cfg(tmp.path().to_path_buf()),
            );
            assert_eq!(
                mgr.classify(&updated),
                ReconcileAction::Spawn,
                "re-queued inbound must spawn immediately"
            );
        }

        /// A clean idle restart — the session was `running` but had no
        /// pending inbound and no in-flight claim — resets quietly. No
        /// notice.
        #[tokio::test(start_paused = true)]
        async fn restart_with_no_in_flight_work_emits_no_notice() {
            let tmp = tempfile::tempdir().unwrap();
            let central = CentralDb::open_in_memory().unwrap();
            let session = fixture_running_session(&central);
            let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
            paths.ensure_dirs().unwrap();

            reset_stale_running_sessions(&central, tmp.path());

            let updated = sessions::get(&central, session.id).unwrap();
            assert!(matches!(updated.container_status, ContainerStatus::Stopped));
            assert!(chat_rows(&paths).is_empty(), "no notice for an idle reset");
        }

        /// Pending inbound that was never picked up (it arrived while
        /// the host was down, or sat unclaimed) is not an interrupted
        /// turn: it will process normally on spawn, so no apology is
        /// owed and no dedupe stamps may be left on the row.
        #[tokio::test(start_paused = true)]
        async fn restart_with_unclaimed_pending_inbound_emits_no_notice() {
            let tmp = tempfile::tempdir().unwrap();
            let central = CentralDb::open_in_memory().unwrap();
            let session = fixture_running_session(&central);
            let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
            paths.ensure_dirs().unwrap();
            let msg_id = seed_routed_inbound(&paths, "hello?", chrono::Duration::zero());
            // No processing_ack claim: no runner ever picked this up.

            reset_stale_running_sessions(&central, tmp.path());

            assert!(chat_rows(&paths).is_empty(), "no turn in flight, no notice");
            // Row untouched: still due, tries not stamped.
            let inbound = open_inbound(&paths).unwrap();
            assert_eq!(messages_in::count_due(&inbound).unwrap(), 1);
            let tries: i64 = inbound
                .query_row(
                    "SELECT tries FROM messages_in WHERE id = ?1",
                    rusqlite::params![msg_id.as_uuid().to_string()],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(tries, 0, "unclaimed rows must not be stamped");
        }

        /// The dedup acceptance: with several inbound rows in flight
        /// the notice still fires once per affected session per boot —
        /// not per inbound row, not per repeat pass, and not again from
        /// the sweep's apology path (the rows are old enough that the
        /// `pending_too_long` apology WOULD fire were the tries stamp
        /// absent).
        #[tokio::test(start_paused = true)]
        async fn notice_fires_once_per_session_not_per_row_pass_or_sweep() {
            use copperclaw_host_sweep::SweepService;
            use copperclaw_host_sweep::service::FilesystemSessionRoot;

            let tmp = tempfile::tempdir().unwrap();
            let central = CentralDb::open_in_memory().unwrap();
            let session = fixture_running_session(&central);
            let paths = SessionPaths::new(tmp.path(), session.agent_group_id, session.id);
            paths.ensure_dirs().unwrap();
            // Two in-flight rows, both past the sweep's 5-minute
            // apology threshold.
            let first = seed_routed_inbound(&paths, "step one", chrono::Duration::minutes(10));
            let second = seed_routed_inbound(&paths, "step two", chrono::Duration::minutes(9));
            claim_processing(&paths, first);
            claim_processing(&paths, second);

            reset_stale_running_sessions(&central, tmp.path());
            assert_eq!(
                chat_rows(&paths).len(),
                1,
                "one notice per session, not per inbound row"
            );

            // Both rows carry the dedupe stamps even though only one
            // notice was written.
            let outbound = open_outbound(&paths).unwrap();
            for id in [first, second] {
                let claim = processing_ack::get(&outbound, id).unwrap().unwrap();
                assert_eq!(claim.status, processing_ack::ProcessingStatus::Failed);
            }

            // A repeat pass over the same state (the session wedged
            // `running` again before anything processed) finds no
            // in-flight claims and stays quiet.
            sessions::mark_container_running(&central, session.id).unwrap();
            reset_stale_running_sessions(&central, tmp.path());
            assert_eq!(chat_rows(&paths).len(), 1, "repeat pass adds nothing");

            // A sweep pass stays out too: the tries stamp keeps the
            // pending_too_long apology away from these old rows, and
            // the Failed claims keep the stale-claim reset out.
            sessions::mark_container_stopped(&central, session.id).unwrap();
            let sweep = SweepService::new(
                central.clone(),
                Arc::new(FilesystemSessionRoot::new(tmp.path())),
            );
            sweep.run_once_actuated().await.unwrap();
            let all_rows = messages_out::list_due(&open_outbound(&paths).unwrap()).unwrap();
            assert_eq!(
                all_rows.len(),
                1,
                "sweep pass must not add apologies on top of the boot notice: {all_rows:?}"
            );
        }
    }
}
